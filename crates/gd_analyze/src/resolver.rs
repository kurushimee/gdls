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
use gd_syntax::Dialect;

use crate::context::AnalysisContext;
use crate::data_type::{
    variant_type_name, DataType, DtKind, MethodSig, ScriptRef, TypeSource, VariantType,
};

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
    let mut first_err = Ok(());
    if recursive {
        for inner in inner_classes(ctx, class_id) {
            // DIALECT(4.7): gdscript_analyzer.cpp resolve_class_inheritance(p_class, p_recursive)
            // — 4.6 returned on the first failing inner class, so its siblings were never resolved
            // and their own inheritance errors never appeared. 4.7 resolves every inner class and
            // reports the first error it saw.
            let inner_err = resolve_class_inheritance_recursive(ctx, inner, true);
            if inner_err.is_err() {
                if ctx.dialect < Dialect::Godot4_7 {
                    return inner_err;
                }
                if first_err.is_ok() {
                    first_err = inner_err;
                }
            }
        }
    }
    first_err
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

    // "Class X hides a …" checks on the class's own name (analyzer.cpp:396-407), all four arms, in
    // Godot's order — the first that matches is the only one reported.
    if let Some(name) = class_identifier_name(ctx, class_id) {
        let id_node = class_identifier(ctx, class_id).unwrap_or(class_id);
        if builtin_type_from_name(&name).is_some() {
            ctx.push_error(format!(r#"Class "{name}" hides a built-in type."#), id_node);
        } else if ctx.native.class_named(&name).is_some() {
            ctx.push_error(format!(r#"Class "{name}" hides a native class."#), id_node);
        } else if hides_global_script_class(ctx, class_id, &name) {
            ctx.push_error(
                format!(r#"Class "{name}" hides a global script class."#),
                id_node,
            );
        } else if ctx.xfile.is_autoload(&name) {
            ctx.push_error(
                format!(r#"Class "{name}" hides an autoload singleton."#),
                id_node,
            );
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
        // #355: Godot's CLASS arm renders the identifier, else the fqcn — which for a head class
        // with no `class_name` is the script's own `res://` path (`-self` reads as
        // `res://src/probe6.gd`). `ctx.script_path` is the last resort: it is a basename in the
        // server, so it is only reached for an un-indexed buffer that has no `res://` path at all.
        display_name: class_identifier_name(ctx, class_id).unwrap_or_else(|| {
            ctx.file
                .and_then(|f| ctx.xfile.res_path(f))
                .unwrap_or_else(|| ctx.script_path.clone())
        }),
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
    let extends = class_extends_names(ctx, class_id);

    // `extends "res://path.gd"` (analyzer.cpp:437-459). Godot tracks how many names the head
    // consumed in `extends_index` — 0 here, because a path base leaves the WHOLE name list as
    // chain segments hanging off the loaded script, and 1 below, where the head is a name. Both
    // then share the segment loop, which is what makes `extends "res://x.gd".Inner` resolve to
    // `Inner` rather than to the file's head class (#384).
    if let Some(path) = class_extends_path(ctx, class_id) {
        // Relative paths resolve against the script's own directory (analyzer.cpp:437); an
        // unresolved path is Godot's "Could not resolve super class path".
        let resolved = match ctx.file {
            Some(from) => ctx.xfile.resolve_path_from(from, &path),
            None => ctx.xfile.resolve_res_path(&path),
        };
        let Some(fid) = resolved else {
            ctx.push_error(
                format!(r#"Could not resolve super class path "{path}"."#),
                class_id,
            );
            return Ok(None);
        };
        let base = script_base_datatype(ctx, fid);
        return resolve_extends_segments(ctx, base, &extends);
    }

    let Some(&first_id) = extends.first() else {
        ctx.push_error("Could not resolve an empty super class path.", class_id);
        return Ok(None);
    };
    let name = ident_name(ctx, first_id).unwrap_or_default();

    // Resolve the head of the `extends` chain.
    let base = if let Some(fid) = ctx.xfile.global_class_file(&name) {
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

    resolve_extends_segments(ctx, base, &extends[1..])
}

/// Nested `extends A.B.C` segments (analyzer.cpp:578-598). For an in-file `Class` base we walk
/// inner classes syntactically; for a cross-file `Script` base (WP-P1) we walk the depended file's
/// inner-class table via `CrossFileQuery::resolve_inner_chain`. Godot's
/// `reduce_identifier_from_base` does both at analyzer.cpp:578-598 — gdls splits the in-file
/// walk from the cross-file walk because the Script base lives outside our parse tree.
///
/// `segments` is whatever the head did not consume, so a path base passes the whole list.
fn resolve_extends_segments(
    ctx: &mut AnalysisContext,
    mut base: DataType,
    segments: &[NodeId],
) -> Result<Option<DataType>, ()> {
    for &id in segments {
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
                // #366: `extends Outer.Inner` in another file. The owning chain is what the
                // segment hangs off, so it is `new_inner` minus this segment — the same
                // `(file, chain, name)` triple the inner class's own declaration answers to.
                let mut owner = new_inner.clone();
                owner.pop();
                record_type_use(
                    ctx,
                    Some(file),
                    owner,
                    crate::binding::BindingTargetKind::Class,
                    &seg,
                    id,
                );
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
                    display_name: script_render_name(ctx, &sref),
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
                if let Some(owner) = declaring_class_path(ctx, inner_id) {
                    record_type_use(
                        ctx,
                        ctx.file,
                        owner,
                        crate::binding::BindingTargetKind::Class,
                        &seg,
                        id,
                    );
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
            if let Some(owner) = declaring_class_path(ctx, look) {
                record_type_use(
                    ctx,
                    ctx.file,
                    owner,
                    crate::binding::BindingTargetKind::Class,
                    name,
                    source,
                );
            }
            return Ok(Some(ctx.get_type(look).clone()));
        }
        match class_member(ctx, look, name) {
            Some(Member::Class(inner_id)) => {
                if ctx.get_type(inner_id).has_no_type() {
                    resolve_class_inheritance(ctx, inner_id, Some(source))?;
                }
                let owner = crate::reducer::class_inner_path(ctx, look);
                record_type_use(
                    ctx,
                    ctx.file,
                    owner,
                    crate::binding::BindingTargetKind::Class,
                    name,
                    source,
                );
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
                    // #366: `class Sub extends Hero:` where `Hero` is a `const … = preload(…)`.
                    // No other surface can see this shape — the server's own extends resolver
                    // never looks at constants — so without this the const's rename abandoned it.
                    let owner = crate::reducer::class_inner_path(ctx, look);
                    record_type_use(
                        ctx,
                        ctx.file,
                        owner,
                        crate::binding::BindingTargetKind::Constant,
                        name,
                        source,
                    );
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

/// What `reduce_identifier_from_base` makes of `name` against a **meta** (class-as-type) base —
/// the three-way switch analyzer.cpp:912-920 keys its two messages off.
///
/// Only a constant, an inner class, an enum, or a *static* function is visible on a meta base; an
/// instance variable, a signal, or an instance function is not there at all and reads as absent.
/// Of the visible ones, a constant is a type only when it holds one (`const Alias = Inner`), and a
/// static function never is. Pinned against `godot --headless --check-only` for every kind.
enum MetaMember {
    /// Resolved to a meta type — the walk continues into it. Boxed: a bare [`DataType`] dwarfs the
    /// two unit variants.
    Type(Box<DataType>),
    /// Resolved, but to a value (analyzer.cpp:918).
    NotAType,
    /// Not on a meta base at all (analyzer.cpp:915).
    Absent,
}

fn meta_member(
    ctx: &mut AnalysisContext,
    class_id: Option<NodeId>,
    name: &str,
    at: NodeId,
) -> MetaMember {
    let Some(class_id) = class_id else {
        return MetaMember::Absent;
    };
    /// Resolve the member in place so its declared type is available, then hand it back.
    fn typed(ctx: &mut AnalysisContext, class_id: NodeId, name: &str, member: NodeId, at: NodeId) {
        if !ctx.get_type(member).has_no_type() {
            return;
        }
        if let Some(idx) = match &ctx.node(class_id).kind {
            NodeKind::Class(c) => c.members_indices.get(name).copied(),
            _ => None,
        } {
            resolve_class_member(ctx, class_id, idx, Some(at));
        }
    }
    match class_member(ctx, class_id, name) {
        Some(Member::Constant(cid)) => {
            typed(ctx, class_id, name, cid, at);
            let dt = ctx.get_type(cid).clone();
            if dt.is_meta_type {
                MetaMember::Type(Box::new(dt))
            } else {
                MetaMember::NotAType
            }
        }
        Some(Member::Enum(eid)) => {
            typed(ctx, class_id, name, eid, at);
            MetaMember::Type(Box::new(ctx.get_type(eid).clone()))
        }
        Some(Member::Function(fid)) => match &ctx.node(fid).kind {
            NodeKind::Function(f) if f.is_static => MetaMember::NotAType,
            _ => MetaMember::Absent,
        },
        _ => MetaMember::Absent,
    }
}

/// `get_class_node_current_scope_classes` (analyzer.cpp:320): the class itself, its (in-file) base
/// chain, and its outer-class chain — deduplicated, in Godot's order.
/// The in-file INHERITANCE chain of `class_id` — the class then each base link, and nothing else.
///
/// The chain half of [`scope_classes`]. Godot gathers the same full scope
/// (`get_class_node_current_scope_classes`, `gdscript_analyzer.cpp:320-344`) but, when
/// `reduce_identifier_from_base` was handed an explicit base, breaks out of the loop the moment it
/// leaves the base chain (`:4270-4275`) — so an outer class is gathered and never reached. This is
/// that walk, expressed directly (#435).
pub(crate) fn chain_classes(ctx: &AnalysisContext, class_id: NodeId) -> Vec<NodeId> {
    let mut out = Vec::new();
    let mut cur = Some(class_id);
    while let Some(node) = cur {
        if out.contains(&node) {
            break;
        }
        out.push(node);
        cur = ctx.base_type(node).class_node;
    }
    out
}

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
fn inherited_enum_annotation(
    ctx: &mut AnalysisContext,
    name: &str,
    first_id: NodeId,
) -> Option<DataType> {
    let class_id = ctx.current_class?;
    if let Some(sr) = crate::reducer::current_class_script_base(ctx) {
        let chain = crate::script_chain::resolve_script_chain(ctx, &sr);
        for link in chain.links.clone() {
            let has = crate::script_chain::link_interface(ctx.xfile, &link)
                .is_some_and(|i| i.enums.iter().any(|e| e.name == name));
            if has && link.inner.is_empty() {
                let dt = crate::reducer::cross_file_named_enum(ctx, link.file, name, true)?;
                // #366: a bare `: Direction` inherited from a base resolves HERE and nowhere else.
                // The identity is the DECLARING file's head class, so a same-named enum in the
                // using file is a different target and stays untouched.
                record_type_use(
                    ctx,
                    Some(link.file),
                    Vec::new(),
                    crate::binding::BindingTargetKind::Enum,
                    name,
                    first_id,
                );
                return Some(dt);
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
    script_ref_datatype(
        ctx,
        ScriptRef {
            file: fid,
            inner: Vec::new(),
        },
    )
}

/// #355: the name [`DataType`]'s `Display` renders for a `Script` kind — Godot's CLASS arm
/// (`gdscript_parser.cpp:5354-5358`): the class's own identifier, else the head class's `fqcn`,
/// which for a GDScript is its `res://` path.
///
/// An inner class always has an identifier, so `inner.last()` wins outright; a head class's
/// identifier is its `class_name`, read off the depended interface. Empty only when the file
/// resolves to no path at all, where `Display` keeps its bracketed placeholder rather than
/// inventing a name.
pub(crate) fn script_render_name(ctx: &AnalysisContext, sref: &ScriptRef) -> String {
    if let Some(seg) = sref.inner.last() {
        return seg.clone();
    }
    if let Some(name) = ctx
        .xfile
        .interface(sref.file)
        .and_then(|i| i.class_name.clone())
    {
        return name;
    }
    // The `res://` spelling is Godot's own fqcn (gdscript_parser.cpp:702) and the only path form
    // safe to show a user — `file_path` is absolute on whatever machine gdls is running on, which
    // on Windows means a drive letter and a home directory name (#419). A file outside the project
    // root has no `res://` form at all; name it by its base name rather than leaking the rest.
    if let Some(res) = ctx.xfile.res_path(sref.file) {
        return res;
    }
    ctx.xfile
        .file_path(sref.file)
        .map(|p| p.rsplit(['/', '\\']).next().unwrap_or(p).to_owned())
        .unwrap_or_default()
}

/// The `Script` meta type for a concrete [`ScriptRef`]: the head script when `inner` is empty, one
/// of its inner classes otherwise. Same shape the cross-file inner-class hop in
/// [`resolve_datatype`] builds for `Outer.Inner` annotations.
pub(crate) fn script_ref_datatype(ctx: &AnalysisContext, sref: ScriptRef) -> DataType {
    DataType {
        kind: DtKind::Script,
        type_source: TypeSource::AnnotatedExplicit,
        is_meta_type: true,
        builtin_type: VariantType::Object,
        native_type: crate::script_chain::chain_native_root(ctx, &sref).unwrap_or_default(),
        display_name: script_render_name(ctx, &sref),
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
            // analyzer.cpp:735. Provenance-gated: the enum set comes from the dump, so under a
            // `Generic` or `Absent` surface a name gdls cannot see is not proof of a typo.
            if ctx.native.provenance() == gd_types::ApiProvenance::Exact {
                ctx.push_error(
                    format!(r#"Name "{seg}" is not a nested type of "Variant"."#),
                    chain[1],
                );
            }
            return bad_type;
        } else if chain.len() > 2 {
            // analyzer.cpp:740 — structural, so no provenance gate: nothing about the engine
            // surface can make a three-segment `Variant.A.B` legal.
            ctx.push_error(
                "Variant only contains enum types, which do not have nested types.".to_owned(),
                chain[2],
            );
            return bad_type;
        }
        result.kind = DtKind::Variant;
    } else if let Some(builtin) = builtin_type_from_name(&first) {
        // Builtin scalar/container, and its nested enums (`Vector3.Axis`).
        if chain.len() == 2 {
            let seg = ident_name(ctx, chain[1]).unwrap_or_default();
            if builtin_has_enum(ctx, builtin, &seg) {
                ctx.set_type(type_id, make_builtin_enum_type(ctx, &seg, builtin, true));
                return ctx.get_type(type_id).clone();
            }
            // analyzer.cpp:754 — same dump-derived negative, same gate as the `Variant` arm.
            if ctx.native.provenance() == gd_types::ApiProvenance::Exact {
                ctx.push_error(
                    format!(r#"Name "{seg}" is not a nested type of "{first}"."#),
                    chain[1],
                );
            }
            return bad_type;
        } else if chain.len() > 2 {
            // analyzer.cpp:758 — structural.
            ctx.push_error(
                "Built-in types only contain enum types, which do not have nested types."
                    .to_owned(),
                chain[2],
            );
            return bad_type;
        }
        result.kind = DtKind::Builtin;
        result.builtin_type = builtin;

        // Container element types (analyzer.cpp:764-783). Godot resolves them HERE, inside the
        // builtin arm, and only for `Array` and `Dictionary` — the arity gate at the tail of this
        // function is a separate, later check. A slot whose resolved type is `Variant` is left
        // unset, so `Array[Variant]` carries no element types at all and renders as plain
        // `Array`, while `Dictionary[Variant, int]` pads slot 0 to reach slot 1.
        let set_slot = |ctx: &mut AnalysisContext, result: &mut DataType, slot: usize| {
            let inner = type_from_metatype(resolve_datatype(ctx, containers.get(slot).copied()));
            if inner.kind == DtKind::Variant {
                return;
            }
            while result.container_element_types.len() <= slot {
                result.container_element_types.push(DataType::variant());
            }
            result.container_element_types[slot] = inner;
        };
        match builtin {
            VariantType::Array => set_slot(ctx, &mut result, 0),
            VariantType::Dictionary => {
                set_slot(ctx, &mut result, 0);
                set_slot(ctx, &mut result, 1);
            }
            _ => {}
        }
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
    } else if let ScopeType::Found(base) = match datatype_in_scope(ctx, &first, first_id) {
        // In-file class in the current scope (analyzer.cpp:847-900, class-name case). A member that
        // exists but names no type has already reported itself and stops the walk — falling through
        // would re-report it as `Could not find type "X" in the current scope.`, which is both a
        // different message and a worse one.
        ScopeType::NotAType => return bad_type,
        other => other,
    } {
        result = *base;
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
    } else if ctx.xfile.is_autoload(&first) {
        // analyzer.cpp:804-823 — a registered singleton autoload with NO backing script (a
        // scriptless SCENE autoload, or an unresolvable scene/uid). Godot's type-position arm
        // early-returns `bad_type` SILENTLY here (`return bad_type` at :822-823): it does NOT fall
        // through to the "Could not find type" error at :902. (Verified against the 4.6.3 binary:
        // `var x: <scriptless-scene-autoload>` compiles with no error, while an unknown type does
        // error.) Unlike the VALUE position — which floors the singleton to a bare `Node`
        // (analyzer.cpp:4570-4609, mirrored by `reduce_identifier` step 9a) — the TYPE position is
        // degenerate: the annotated variable just stays untyped (VARIANT). Return bad_type to match.
        return bad_type;
    } else if let Some(dt) = inherited_enum_annotation(ctx, &first, first_id) {
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
        //
        // v1.0.2 deliberate deviation (issue #24, same rule as the cross-file Script-segment
        // walk below): this negative claim is only trustworthy when the native surface came
        // from the project's own engine. Under a `Generic` (embedded stock fallback) or
        // `Absent` (no source at all) DB, a custom engine build's class is indistinguishable
        // from a typo — Godot itself can never be in this state, so fidelity doesn't bind here;
        // degrade to a silent Variant per the docs/00 "unknown stays dynamic" rule.
        //
        // The same soundness bar has a per-class carve-out: a dump generated without
        // extension registration (a failed DLL load silently unregisters the rest; a
        // never-imported project) is engine-`Exact` yet blind to classes Godot's own ClassDB
        // carries. When the project itself declares the name via a GDExtension, "Could not
        // find type" is exactly as unsound as the provenance cases above — degrade silently.
        if ctx.native.provenance() != gd_types::ApiProvenance::Exact
            || ctx.native.is_extension_declared_missing(&first)
        {
            return bad_type;
        }
        ctx.push_error(
            format!(r#"Could not find type "{first}" in the current scope."#),
            first_id,
        );
        return bad_type;
    }

    // Nested `A.B` segments under an in-file `Class` base (analyzer.cpp:908-939). Godot runs
    // `reduce_identifier_from_base` per segment and then sorts the outcome three ways: unset is
    // :915, set-but-not-meta is :918, meta continues the walk. `meta_member` is that switch.
    if chain.len() > 1 && result.kind == DtKind::Class {
        for &id in &chain[1..] {
            let seg = ident_name(ctx, id).unwrap_or_default();
            if result.kind == DtKind::Enum {
                // The walk stepped onto an enum. Godot keeps calling
                // `reduce_identifier_from_base`, which on an enum base resolves the enum's own
                // values (non-meta, so :918) and nothing else (:915).
                let base_name = result.to_string();
                let msg = if result.enum_values.contains_key(&seg) {
                    format!(r#"Member "{seg}" under base "{base_name}" is not a valid type."#)
                } else {
                    format!(r#"Could not find type "{seg}" under base "{base_name}"."#)
                };
                ctx.push_error(msg, id);
                return bad_type;
            }
            let parent_class_node = result.class_node;
            if let Some(inner_id) = parent_class_node.and_then(|c| inner_class_named(ctx, c, &seg))
            {
                if ctx.get_type(inner_id).has_no_type() {
                    let _ = resolve_class_inheritance(ctx, inner_id, Some(id));
                }
                if let Some(owner) = declaring_class_path(ctx, inner_id) {
                    record_type_use(
                        ctx,
                        ctx.file,
                        owner,
                        crate::binding::BindingTargetKind::Class,
                        &seg,
                        id,
                    );
                }
                result = ctx.get_type(inner_id).clone();
                continue;
            }
            // The base is an in-file `Class` (we walked here from one), so the identifier is
            // concrete and `class_identifier_name_or_default` renders it the way Godot's
            // `DataType::to_string()` does. Cross-file Script parents go through the
            // `interface()` walk below and never reach this arm.
            let base_name = crate::reducer::class_identifier_name_or_default(ctx, &result);
            match meta_member(ctx, parent_class_node, &seg, id) {
                MetaMember::Type(dt) => {
                    // #366: an in-file qualified suffix (`var x: Owner.Hero`) whose segment is a
                    // constant or enum rather than an inner class. The kind comes from the member
                    // table, since a `Member` target's collection is binding-only.
                    if let Some(kind) = parent_class_node
                        .and_then(|c| class_member(ctx, c, &seg))
                        .map(|m| match m {
                            Member::Class(_) => crate::binding::BindingTargetKind::Class,
                            Member::Enum(_) => crate::binding::BindingTargetKind::Enum,
                            _ => crate::binding::BindingTargetKind::Constant,
                        })
                    {
                        let owner = parent_class_node
                            .map(|c| crate::reducer::class_inner_path(ctx, c))
                            .unwrap_or_default();
                        record_type_use(ctx, ctx.file, owner, kind, &seg, id);
                    }
                    result = *dt;
                }
                MetaMember::NotAType => {
                    // analyzer.cpp:918.
                    ctx.push_error(
                        format!(r#"Member "{seg}" under base "{base_name}" is not a valid type."#),
                        id,
                    );
                    return bad_type;
                }
                MetaMember::Absent => {
                    // analyzer.cpp:915.
                    ctx.push_error(
                        format!(r#"Could not find type "{seg}" under base "{base_name}"."#),
                        id,
                    );
                    return bad_type;
                }
            }
        }
    } else if chain.len() > 1 && result.kind == DtKind::Script {
        // Cross-file nested types under a global-class / autoload head: an enum leaf
        // (`-> BaseLayer.BlendModes`) or inner-class hops (`Keychain.InputAction`). Godot
        // resolves these through the depended parser's members (analyzer.cpp:908-939); gdls
        // walks the interface chain.
        //
        // #299: the segment lookup now goes through `lookup_script_chain_member`, the same walk
        // the expression path uses. That buys three things over the old hand-rolled probe: enums
        // and inner classes INHERITED from a script base resolve (the old code only looked at the
        // named link itself), an enum leaf under an inner class resolves (the old enum probe was
        // gated on `sr.inner.is_empty()`), and the walk records a `Binding::Use` so `definition` /
        // `references` can address a nested type named in type position.
        for &id in &chain[1..] {
            let seg = ident_name(ctx, id).unwrap_or_default();
            if result.kind == DtKind::Enum {
                // Same enum-base step as the in-file `Class` arm above: an enum's own value
                // resolves but is not a type (analyzer.cpp:918), and nothing else resolves
                // at all (:915).
                let base_name = result.to_string();
                let msg = if result.enum_values.contains_key(&seg) {
                    format!(r#"Member "{seg}" under base "{base_name}" is not a valid type."#)
                } else {
                    format!(r#"Could not find type "{seg}" under base "{base_name}"."#)
                };
                ctx.push_error(msg, id);
                return bad_type;
            }
            let Some(sr) = result.script_type.clone() else {
                return bad_type;
            };
            if let Some((dt, _fold, kind)) =
                crate::reducer::lookup_script_chain_member(ctx, &sr, &seg, true, id)
            {
                // Only a TYPE may appear in a type-annotation chain.
                match kind {
                    crate::binding::BindingTargetKind::Class => {
                        result = dt;
                        continue;
                    }
                    // An enum may step into the walk; the enum-base arm at the loop head is
                    // what rejects a segment under it.
                    crate::binding::BindingTargetKind::Enum => {
                        result = dt;
                        continue;
                    }
                    _ => {
                        // analyzer.cpp:918 — a member that exists under the base but is not a
                        // type. `is_meta_type` is upstream's own test, so a constant that does
                        // hold a type is excluded here exactly as it is there. Same soundness bar
                        // as the miss below: only an `Exact` dump over a fully walked chain makes
                        // the claim provable.
                        if !dt.is_meta_type
                            && ctx.native.provenance() == gd_types::ApiProvenance::Exact
                            && crate::script_chain::chain_native_root(ctx, &sr).is_some()
                        {
                            let base_name =
                                crate::reducer::class_identifier_name_or_default(ctx, &result);
                            ctx.push_error(
                                format!(
                                    r#"Member "{seg}" under base "{base_name}" is not a valid type."#
                                ),
                                id,
                            );
                        }
                        return bad_type;
                    }
                }
            }
            // analyzer.cpp:915 — `Could not find type "X" under base "Y".` Same soundness bar as
            // the member miss in `reduce_identifier_from_base` (#299): the negative is only
            // provable when gdls saw the base's whole surface, which means an `Exact` dump and a
            // chain that was fully walkable (`chain_native_root` is `Some` — `None` means some
            // link's interface was missing, its head unresolvable, or a cycle closed, and
            // `script_chain`'s module doc requires consumers to stay permissive there). Under
            // `NoCrossFile` no interface resolves, so the corpus never reaches this arm.
            if ctx.native.provenance() == gd_types::ApiProvenance::Exact
                && crate::script_chain::chain_native_root(ctx, &sr).is_some()
            {
                let base_name = crate::reducer::class_identifier_name_or_default(ctx, &result);
                ctx.push_error(
                    format!(r#"Could not find type "{seg}" under base "{base_name}"."#),
                    id,
                );
            }
            return bad_type;
        }
    } else if chain.len() > 1 && result.kind == DtKind::Native {
        // analyzer.cpp:922-934 — `TileSet.TileShape` style: a native class followed by exactly one
        // segment that names an enum on that class (or one of its bases).
        let seg = ident_name(ctx, chain[1]).unwrap_or_default();
        if !crate::reducer::native_has_enum(ctx, &result.native_type, &seg) {
            // analyzer.cpp:931. Provenance-gated, exactly like the script-chain arm above: the
            // enum set is what the dump carries, so under `Generic`/`Absent` a name gdls cannot
            // see is indistinguishable from a custom build's.
            if ctx.native.provenance() == gd_types::ApiProvenance::Exact {
                ctx.push_error(
                    format!(r#"Could not find type "{seg}" in "{first}"."#),
                    chain[1],
                );
            }
            return bad_type;
        }
        if chain.len() > 2 {
            // analyzer.cpp:926 — structural: an enum has no nested types, whatever the surface.
            ctx.push_error("Enums cannot contain nested types.".to_owned(), chain[2]);
            return bad_type;
        }
        result = make_native_enum_type(ctx, &seg, &result.native_type, true);
    } else if chain.len() > 1 {
        // analyzer.cpp:935's `else` — the head resolved to something that is neither a class, a
        // script, nor a native: an enum (`MyEnum.A` in type position), a builtin instance type,
        // a Variant. None of those carry nested types. Structural, so ungated.
        let seg = ident_name(ctx, chain[1]).unwrap_or_default();
        let base_name = result.to_string();
        ctx.push_error(
            format!(r#"Could not find nested type "{seg}" under base "{base_name}"."#),
            chain[1],
        );
        return bad_type;
    }

    // Container element-type arity (analyzer.cpp:941-957). The element types themselves were
    // stamped up in the builtin arm; this gate runs last, after the whole nested-type walk, and
    // rejects the annotation outright rather than truncating it. `Array[int, String]` is a real
    // typo for `Dictionary[int, String]`, and answering it as `Array[int]` would make every later
    // hover, completion, and assignment check agree on a type the source never asked for.
    if !containers.is_empty() {
        let arity_error = match (result.kind, result.builtin_type) {
            (DtKind::Builtin, VariantType::Array) if containers.len() != 1 => {
                Some("Typed arrays require exactly one collection element type.")
            }
            (DtKind::Builtin, VariantType::Dictionary) if containers.len() != 2 => {
                Some("Typed dictionaries require exactly two collection element types.")
            }
            (DtKind::Builtin, VariantType::Array | VariantType::Dictionary) => None,
            _ => Some("Only arrays and dictionaries can specify collection element types."),
        };
        if let Some(msg) = arity_error {
            ctx.push_error(msg.to_owned(), type_id);
            return bad_type;
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

/// analyzer.cpp:402 — whether `class_id`'s own name shadows a project `class_name` REGISTERED
/// SOMEWHERE ELSE. The self-exclusion is the whole subtlety: the head class of `foo.gd` declaring
/// `class_name Foo` is what put `Foo` in the registry, so it must not report itself, while an INNER
/// class named `Foo` inside that same file still does. Godot spells that as "the registered path is
/// not this script's path, OR this is not the head class"; gdls compares file IDS rather than path
/// strings, which is the same identity and immune to the `res://` versus absolute-path spelling the
/// two sides use.
///
/// An un-indexed buffer (`ctx.file` is `None`) has no identity to exclude by, so it reports only
/// when the name is registered to some other file — never against itself, since it is in no
/// registry.
fn hides_global_script_class(ctx: &AnalysisContext, class_id: NodeId, name: &str) -> bool {
    let Some(registered) = ctx.xfile.global_class_file(name) else {
        return false;
    };
    let is_head = class_outer(ctx, class_id).is_none();
    Some(registered) != ctx.file || !is_head
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
/// The outcome of the in-scope type lookup ([`datatype_in_scope`]). Godot's switch over the matched
/// member's kind has three outcomes, not two: a type, no member at all, and *a member that is not a
/// type* — which is an error in its own right (analyzer.cpp:894), not a reason to keep looking.
enum ScopeType {
    /// A member (or class) named the type. Boxed: a bare [`DataType`] dwarfs the two unit variants.
    Found(Box<DataType>),
    /// A member with this name exists but cannot name a type; the error is already pushed.
    NotAType,
    /// Nothing in scope carries this name — resolution continues down the chain.
    Absent,
}

/// Record a [`Binding::Use`] for a name that resolved in TYPE position — a type-annotation head,
/// an `extends` head, or an `extends` chain segment (#366).
///
/// These positions resolve here and nowhere else. `references` (and `rename` through it) is
/// identity-keyed on the DECLARING `(file, inner-class chain, name)` triple, so a resolved
/// position that records nothing is a position a rename silently leaves behind — which is how a
/// `const Hero = preload(…)` rename used to rewrite `Hero.new()` and abandon `var h: Hero` in the
/// same file. Recording here inherits the faithful arm order above, so a name that binds something
/// else records that something else, or nothing: a shadowing global `class_name` beats a
/// class-scope member exactly as it does in Godot, and the position is left alone.
fn record_type_use(
    ctx: &mut AnalysisContext,
    target_file: Option<gd_project::FileId>,
    class_path: Vec<String>,
    kind: crate::binding::BindingTargetKind,
    name: &str,
    site: NodeId,
) {
    let span = ctx.node(site).span;
    ctx.record_binding(crate::binding::Binding::use_(
        target_file,
        class_path,
        kind,
        name.to_owned(),
        span,
    ));
}

/// The chain of the class DECLARING `class_id`, i.e. `class_inner_path` minus the class's own
/// name. `None` for a file's root class, whose rename is a global-class rename with its own path.
fn declaring_class_path(ctx: &AnalysisContext, class_id: NodeId) -> Option<Vec<String>> {
    let mut path = crate::reducer::class_inner_path(ctx, class_id);
    path.pop()?;
    Some(path)
}

fn datatype_in_scope(ctx: &mut AnalysisContext, name: &str, first_id: NodeId) -> ScopeType {
    let Some(current) = ctx.current_class else {
        return ScopeType::Absent;
    };
    let scope = scope_classes(ctx, current);
    for look in scope.iter().copied() {
        if class_identifier_name(ctx, look).as_deref() == Some(name) {
            if let Some(owner) = declaring_class_path(ctx, look) {
                record_type_use(
                    ctx,
                    ctx.file,
                    owner,
                    crate::binding::BindingTargetKind::Class,
                    name,
                    first_id,
                );
            }
            return ScopeType::Found(Box::new(ctx.get_type(look).clone()));
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
                let owner = crate::reducer::class_inner_path(ctx, look);
                record_type_use(
                    ctx,
                    ctx.file,
                    owner,
                    crate::binding::BindingTargetKind::Class,
                    name,
                    first_id,
                );
                return ScopeType::Found(Box::new(ctx.get_type(inner_id).clone()));
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
                let owner = crate::reducer::class_inner_path(ctx, look);
                record_type_use(
                    ctx,
                    ctx.file,
                    owner,
                    crate::binding::BindingTargetKind::Enum,
                    name,
                    first_id,
                );
                return ScopeType::Found(Box::new(ctx.get_type(enum_id).clone()));
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
                let mut const_dt = ctx.get_type(const_id).clone();
                // `const X: Script = preload("…")` used as a TYPE: the explicit `Script`
                // annotation hides the preload's Script-meta — but Godot's type usage reads
                // the constant's reduced VALUE (the loaded script class), so prefer the
                // initializer's meta when the annotated type isn't one.
                if !const_dt.is_meta_type {
                    if let NodeKind::Constant(c) = ctx.node(const_id).kind.clone() {
                        if let Some(init) = c.initializer {
                            crate::reducer::reduce_expression(ctx, init, false);
                            let init_dt = ctx.get_type(init).clone();
                            if init_dt.is_meta_type
                                && matches!(init_dt.kind, DtKind::Script | DtKind::Class)
                            {
                                const_dt = init_dt;
                            }
                        }
                    }
                }
                // analyzer.cpp:874-894 — a CONSTANT names a type only when its datatype is a
                // meta type, or its reduced value is a Script. Anything else falls through to the
                // `default:` arm's `"X" is a <kind> but does not contain a type.`
                //
                // gdls reports that only when it POSITIVELY knows the constant holds a non-type
                // value: a hard, set, non-meta type. A `Variant` or unresolved constant is the
                // shape a preload chain gdls could not follow lands in, and claiming *that* is not
                // a type would false-positive on exactly the cross-file cases the "unknown stays
                // dynamic" rule exists for (`features/local_const_as_type.gd`'s `const O =
                // preload(...)` style).
                if const_dt.is_meta_type {
                    let owner = crate::reducer::class_inner_path(ctx, look);
                    record_type_use(
                        ctx,
                        ctx.file,
                        owner,
                        crate::binding::BindingTargetKind::Constant,
                        name,
                        first_id,
                    );
                    return ScopeType::Found(Box::new(const_dt));
                }
                if const_dt.is_set() && !const_dt.is_variant() && !const_dt.has_no_type() {
                    ctx.push_error(
                        format!(r#""{name}" is a constant but does not contain a type."#),
                        first_id,
                    );
                    return ScopeType::NotAType;
                }
                if const_dt.is_set() {
                    let owner = crate::reducer::class_inner_path(ctx, look);
                    record_type_use(
                        ctx,
                        ctx.file,
                        owner,
                        crate::binding::BindingTargetKind::Constant,
                        name,
                        first_id,
                    );
                    return ScopeType::Found(Box::new(const_dt));
                }
            }
            // analyzer.cpp:894's `default:` — a variable / function / signal / enum value / group
            // named the type. The member exists, so this is not "could not find it": it is the
            // wrong KIND of member, and Godot says so. Purely in-file evidence, so no provenance
            // gate applies.
            Some(other) => {
                let kind = member_kind_name(other);
                ctx.push_error(
                    format!(r#""{name}" is a {kind} but does not contain a type."#),
                    first_id,
                );
                return ScopeType::NotAType;
            }
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
                        // Found an inner class with this name in a cross-file base. Point the
                        // ScriptRef at that inner class, the way analyzer.cpp:862-868's CLASS arm
                        // returns `member.m_class`'s own datatype. Returning the base script here
                        // instead made the name resolve as a valid annotation but with the WRONG
                        // type, so `var v: Inner = Inner.new()` failed its assignment check
                        // against the base script (#284).
                        let mut inner: Vec<String> = link.inner.clone();
                        inner.push(name.to_string());
                        record_type_use(
                            ctx,
                            Some(link.file),
                            link.inner.clone(),
                            crate::binding::BindingTargetKind::Class,
                            name,
                            first_id,
                        );
                        return ScopeType::Found(Box::new(script_ref_datatype(
                            ctx,
                            ScriptRef {
                                file: link.file,
                                inner,
                            },
                        )));
                    }
                }
            }
        }
    }
    ScopeType::Absent
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

    // REDUNDANT_STATIC_UNLOAD (analyzer.cpp:1275, 1318-1338): `@static_unload` on a class with
    // no static data. The static-data flag is the parser-side one — a `static var` member or a
    // `static func _static_init` — OR'd over the class itself and its *direct* inner classes
    // (each inner contributes only its own flag, exactly as upstream reads
    // `member.m_class->has_static_data`). Anchored at the `@static_unload` annotation.
    let annotated_static_unload = find_class_annotation(ctx, class_id, "@static_unload");
    if let Some(ann_id) = annotated_static_unload {
        let mut has_static_data = class_has_static_data(ctx, class_id);
        if !has_static_data {
            for i in 0..member_count(ctx, class_id) {
                if let Some(Member::Class(inner)) = nth_member(ctx, class_id, i) {
                    if class_has_static_data(ctx, inner) {
                        has_static_data = true;
                        break;
                    }
                }
            }
        }
        if !has_static_data {
            ctx.push_warning(
                crate::warnings::WarningCode::RedundantStaticUnload,
                &[],
                ann_id,
            );
        }
    }
}

/// The parser-side `ClassNode::has_static_data` flag, re-derived from the tree: a `static var`
/// member (gdscript_parser.cpp:1103-1106) or a `static func _static_init` constructor
/// (gdscript_parser.cpp:1725-1729). Non-recursive — callers OR direct inner classes themselves,
/// mirroring the analyzer's read of each inner's own flag.
fn class_has_static_data(ctx: &AnalysisContext, class_id: NodeId) -> bool {
    for i in 0..member_count(ctx, class_id) {
        match nth_member(ctx, class_id, i) {
            Some(Member::Variable(v)) => {
                if matches!(&ctx.node(v).kind, NodeKind::Variable(var) if var.is_static) {
                    return true;
                }
            }
            Some(Member::Function(f)) => {
                if let NodeKind::Function(func) = &ctx.node(f).kind {
                    if func.is_static && decl_identifier_name(ctx, f) == "_static_init" {
                        return true;
                    }
                }
            }
            _ => {}
        }
    }
    false
}

/// The class's annotation node named `name`, if any — e.g. `@static_unload`
/// (the anchor Godot picks at analyzer.cpp:1330-1336).
fn find_class_annotation(ctx: &AnalysisContext, class_id: NodeId, name: &str) -> Option<NodeId> {
    for &ann_id in &ctx.node(class_id).annotations {
        if let NodeKind::Annotation(a) = &ctx.node(ann_id).kind {
            if a.name == name {
                return Some(ann_id);
            }
        }
    }
    None
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
            // analyzer.cpp:2124-2133 — the same constant-expression check the local arm runs. A
            // class-level `const` had none at all, so `const A = Node` was accepted silently and
            // then flowed on as a type alias (#338's meta-constant arm reads such a constant back
            // as a type).
            if let Some(init_id) = init {
                if const_init_nonconstant_ref(ctx, init_id).is_some() {
                    ctx.push_error(
                        format!(
                            r#"Assigned value for constant "{name}" isn't a constant expression."#
                        ),
                        init_id,
                    );
                }
            }
            // analyzer.cpp:1115-1119 — apply the constant's annotations after resolving it.
            resolve_node_annotations(ctx, id);
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
            // analyzer.cpp:1145-1149.
            resolve_node_annotations(ctx, id);
        }
        Member::Enum(id) => {
            let name = decl_identifier_name(ctx, id);
            check_class_member_name_conflict(ctx, class_id, &name, false, id);
            ctx.set_type(id, resolving());
            let enum_type = resolve_enum_type(ctx, id, class_id, &name);
            ctx.set_type(id, enum_type);
            // analyzer.cpp:1200-1204.
            resolve_node_annotations(ctx, id);
        }
        Member::Function(id) => {
            // Functions are not conflict-checked (they may override a parent function).
            // Apply function-level annotations BEFORE signature resolution
            // (analyzer.cpp:1206-1209) — this is what makes `@abstract`'s static-misuse and
            // duplicate-on-function errors interleave with Godot's class-level @abstract
            // emissions in the right order.
            apply_function_annotations(ctx, id);
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
                let mut resolved = true;
                if let Some(cv) = ev.custom_value {
                    // analyzer.cpp:1218-1221 — an unnamed enum's values are hoisted to class
                    // members, so the block a sibling reference may reach comes from the value's
                    // own `parent_enum` back-pointer rather than from a surrounding loop.
                    let prev_enum = ctx.current_enum;
                    ctx.current_enum = ev.parent_enum;
                    let errors_before = ctx.diagnostic_count();
                    crate::reducer::reduce_expression(ctx, cv, false);
                    ctx.current_enum = prev_enum;
                    let raised = ctx.diagnostic_count() > errors_before;
                    // analyzer.cpp:1223-1231. gdls's fold table can't tell "not constant" from
                    // "constant we don't model", so the emission is gated on the reduction having
                    // raised an error, the same hedge `resolve_enum_type` uses.
                    match ctx.folds.get(cv).cloned() {
                        Some(crate::foldtable::FoldedValue::Int(_)) => {}
                        Some(_) => {
                            ctx.push_error("Enum values must be integers.", cv);
                            resolved = false;
                        }
                        None => {
                            if raised {
                                ctx.push_error("Enum values must be constant.", cv);
                            }
                            resolved = false;
                        }
                    }
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
                if resolved {
                    ctx.enum_element_values.insert(iid, 0);
                }
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
        // analyzer.cpp:1801-1820 — the Array validation applies only when a type IS specified;
        // an untyped rest parameter is an *inferred* `Array` plus an UNTYPED_DECLARATION
        // warning, never an error (validating the unspecified shape false-positived
        // `func f(...args):` with `…but "Variant" is specified` through v1.0.2).
        let has_specifier = matches!(
            &ctx.node(rp).kind,
            NodeKind::Parameter(p) if p.datatype_specifier.is_some()
        );
        if has_specifier {
            let rt = ctx.get_type(rp).clone();
            if rt.is_set() {
                let is_array = rt.kind == DtKind::Builtin && rt.builtin_type == VariantType::Array;
                if !is_array {
                    ctx.push_error(
                        format!(
                            r#"The rest parameter type must be "Array", but "{rt}" is specified."#
                        ),
                        rp,
                    );
                } else if !rt.container_element_types.is_empty() {
                    ctx.push_error(
                        "Typed arrays are currently not supported for the rest parameter.",
                        rp,
                    );
                }
            }
        } else {
            ctx.set_type(
                rp,
                DataType {
                    type_source: TypeSource::Inferred,
                    kind: DtKind::Builtin,
                    builtin_type: VariantType::Array,
                    ..Default::default()
                },
            );
            // The dedicated vararg warning at analyzer.cpp:1817 — in addition to the generic
            // one `resolve_assignable` queued for the same node, exactly as upstream.
            let rp_name = decl_identifier_name(ctx, rp);
            ctx.push_warning(
                crate::warnings::WarningCode::UntypedDeclaration,
                &["Parameter".to_owned(), rp_name],
                rp,
            );
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
            enclosing_native_base(ctx)
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

    // analyzer.cpp:1966-1969 — UNTYPED_DECLARATION on a function without an explicit return
    // type (constructors included, as upstream). `function_visible_name` (analyzer.cpp:1772-
    // 1775): the empty name here means a lambda — named declarations always carry an
    // identifier by the time signature resolution runs.
    if return_type.is_none() {
        let visible_name = if name.is_empty() {
            "<anonymous lambda>".to_owned()
        } else {
            name.clone()
        };
        ctx.push_warning(
            crate::warnings::WarningCode::UntypedDeclaration,
            &["Function".to_owned(), visible_name],
            func_id,
        );
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
    // DIALECT(4.7): gdscript_analyzer.cpp resolve_function_signature() — 4.6 left an untyped
    // override at soft Variant, so its body was never checked against the parent's contract.
    // 4.7 adopts the parent's return type (GH-118877).
    if ctx.dialect < Dialect::Godot4_7 {
        return;
    }
    let Some(current_class) = ctx.current_class else {
        return;
    };
    let Some(parent_return) = parent_return_type(ctx, current_class, name) else {
        return;
    };
    let child_return = ctx.get_type(func_id).clone();
    if !child_return.is_hard_type() {
        ctx.set_type(func_id, parent_return);
    }
}

/// `get_function_signature`'s return-type half (analyzer.cpp:5905-6015): the in-file script chain
/// first, then the native base's ClassDB entry.
fn parent_return_type(
    ctx: &AnalysisContext,
    current_class: NodeId,
    name: &str,
) -> Option<DataType> {
    if let Some(parent_fn) = find_parent_function(ctx, current_class, name) {
        return Some(function_signature(ctx, parent_fn).return_type);
    }
    // Godot keeps walking into `ClassDB` when no script ancestor declares the method, so an
    // untyped `func _ready():` inherits `Node._ready`'s `void`. Same suppression gate as
    // NATIVE_METHOD_OVERRIDE above: only the class whose resolution actually reaches the native
    // method adopts from it.
    if script_ancestor_defines_function(ctx, name) {
        return None;
    }
    let native = enclosing_native_base(ctx)?;
    let sig = crate::reducer::lookup_native_method(ctx, &native, name)?;
    // The GH-118877 compatibility exception: an untyped `_get_property_list` override keeps a
    // plain `Array` rather than the declared `Array[Dictionary]`, because the mismatch can only
    // be detected at runtime and too much existing code returns an untyped array.
    if name == "_get_property_list" && sig.is_virtual {
        return Some(DataType {
            type_source: TypeSource::AnnotatedInferred,
            kind: DtKind::Builtin,
            builtin_type: VariantType::Array,
            ..Default::default()
        });
    }
    Some(sig.return_dt)
}

/// The `ClassDB` half of `get_function_signature` (analyzer.cpp:5905-6015), shaped as a
/// [`FunctionSig`] so the override check can compare against a native virtual the script chain
/// never redeclares — `Object._get_property_list`, `Node._process`, and the rest.
///
/// Gated the same way [`parent_return_type`] is: a script ancestor that declares the name owns the
/// contract, and only the class whose base chain actually reaches the native method compares
/// against it. A `seed_dump_omitted_methods` stub is skipped outright — its empty parameter list
/// is a name-only placeholder, so comparing arity against it would invent a mismatch.
fn native_parent_signature(ctx: &AnalysisContext, name: &str) -> Option<FunctionSig> {
    if script_ancestor_defines_function(ctx, name) {
        return None;
    }
    let native = enclosing_native_base(ctx)?;
    let sig = crate::reducer::lookup_native_method(ctx, &native, name)?;
    if !sig.arity_known {
        return None;
    }
    // `default_par_count` is the trailing run the dump marks with a default value; gdls's
    // `min_params` already counts the ones without.
    let total = sig.par_types.len().max(sig.max_params);
    let mut has_default = vec![false; total];
    for slot in has_default.iter_mut().skip(sig.min_params) {
        *slot = true;
    }
    Some(FunctionSig {
        return_type: sig.return_dt,
        param_types: sig.par_types,
        has_default,
        is_vararg: sig.is_vararg,
    })
}

/// Emit the override-signature-mismatch error (body-pass step). Runs after the body so the
/// emission lands AFTER any sibling functions' interface-pass errors — matching Godot's
/// observed emission order in corpus `.out` files (analyzer.cpp:1865-1960).
fn check_override_signature(ctx: &mut AnalysisContext, func_id: NodeId, name: &str) {
    let Some(current_class) = ctx.current_class else {
        return;
    };
    // Godot resolves the parent through one `get_function_signature` call, which walks the script
    // chain and then falls into `ClassDB` (analyzer.cpp:1875). Only the script half yields a
    // `parent_fn`, and only that half can be the cyclic-override case below.
    let parent_fn = find_parent_function(ctx, current_class, name);
    let parent = match parent_fn {
        Some(f) => function_signature(ctx, f),
        None => match native_parent_signature(ctx, name) {
            Some(p) => p,
            None => return,
        },
    };

    let child = function_signature(ctx, func_id);

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
        && parent_fn.is_some_and(|f| parent_default_references_class(ctx, f, &current_class_name))
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
fn enclosing_native_base(ctx: &AnalysisContext) -> Option<String> {
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

/// clang's note label for the shadowed-declaration related location every SHADOWED_* emission
/// attaches — the structured, navigable twin of the "at line N" the message bakes into text.
/// Not a Godot string: Godot never serializes related locations, so the label carries no
/// fidelity constraint.
const PREVIOUS_DECL_LABEL: &str = "previous declaration is here";

/// The identifier span of a member-declaration node — the narrow related-location anchor (the
/// declaration's name token, not the whole node). `None` when the declaration has no
/// identifier (recovered parse); callers emit the plain warning rather than a junk span.
fn member_decl_ident_span(
    ctx: &AnalysisContext,
    member_node: NodeId,
) -> Option<gd_syntax::ByteSpan> {
    let id = match &ctx.node(member_node).kind {
        NodeKind::Class(c) => c.identifier,
        NodeKind::Constant(c) => c.identifier,
        NodeKind::Function(f) => f.identifier,
        NodeKind::Signal(s) => s.identifier,
        NodeKind::Variable(v) => v.identifier,
        NodeKind::Enum(e) => e.identifier,
        _ => None,
    }?;
    Some(ctx.node(id).span)
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
    let related = member_decl_ident_span(ctx, member_node)
        .map(|span| {
            vec![crate::diagnostic::RelatedInfo {
                file: None,
                span,
                message: PREVIOUS_DECL_LABEL.to_owned(),
            }]
        })
        .unwrap_or_default();
    ctx.push_warning_with_related(
        crate::warnings::WarningCode::ShadowedVariable,
        &[
            "function parameter".to_owned(),
            name,
            member_kind,
            member_line,
        ],
        identifier_id,
        related,
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
        let pre_diag_count = ctx.error_count();
        crate::reducer::reduce_expression(ctx, init, false);
        if is_constant {
            // analyzer.cpp:2126 — a `const` initializer is folded through
            // `make_expression_reduced_value`, which reaches the array and dictionary literals the
            // plain reducer leaves valueless. #385.
            crate::reducer::fold_constant_site(ctx, init);
        }
        let initializer_type = ctx.get_type(init).clone();
        let init_emitted_errors = ctx.error_count() > pre_diag_count;

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
            // An operator-node initializer (binary/unary/ternary) with a soft NON-Variant
            // result is trustworthy: the operator reducers compute hardness faithfully from
            // their operands, and gdls's under-hard-typing degrades always come out as
            // Variant-kinded (which the variant arms keep silent).
            let init_is_operator = matches!(
                ctx.node(init).kind,
                NodeKind::BinaryOp(_) | NodeKind::UnaryOp(_) | NodeKind::TernaryOp(_)
            );
            let weak_type_safe = !init_emitted_errors
                && !initializer_type.is_hard_type()
                && initializer_type.is_set()
                && !initializer_type.has_no_type()
                && ((init_is_plain_identifier && !init_name_starts_upper)
                    || (init_is_operator && initializer_type.kind != DtKind::Variant));
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
                // `func f(p := variant())`), not the parameter itself. The function span in
                // `warning_ignored_lines` (annotation line through signature end) covers every
                // parameter line, so the plain line filter suppresses it — same as upstream.
                ctx.push_warning(
                    crate::warnings::WarningCode::InferenceOnVariant,
                    &[kind_label.to_owned()],
                    node_id,
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
            // Read before the move: the bit records what the initializer was, and both the
            // `drops_to_variant` rewrite below and the source stamp change the answer. A
            // `Resolving` initializer dropped to Variant must read untrusted (its error is
            // already out); a hard `Nil` one dropped to Variant must read trusted, since
            // `var x = null` then `x.p` is an inference failure upstream too.
            let init_is_dynamic = initializer_type.is_positively_dynamic();
            ty = initializer_type;
            let drops_to_variant = !ty.is_set()
                || (ty.is_hard_type()
                    && ty.kind == DtKind::Builtin
                    && ty.builtin_type == VariantType::Nil
                    && !is_constant);
            if drops_to_variant {
                ty.kind = DtKind::Variant;
            }
            // Upstream promotes unconditionally (analyzer.cpp:2150-2154), but it only reaches
            // this line on the `:=` path after erroring on ANY non-hard initializer
            // (analyzer.cpp:2141's `!is_hard_type()` clause), so upstream a hard
            // `AnnotatedInferred` local is always backed by a hard initializer. gdls holds that
            // clause back (`weak_type_safe` above) because the reducer degrades an unresolvable
            // cross-file chain to a SOFT `Inferred` Variant — and promoting THAT would launder
            // the degrade into a hard Variant, which the operator reducers' trust guards read as
            // a genuine dynamic and stamp `Undetected` on, firing a false `Cannot infer …` one
            // use later. So a soft Variant stays soft: "unknown stays dynamic" (docs/00) has to
            // survive the declaration. The state this defines is one upstream never reaches
            // without an error already emitted, so no upstream behavior is overridden.
            //
            // A PARAMETER is excluded, and that exclusion is load-bearing: a `:=` parameter
            // default that degrades (the cyclic override default in `errors/cyclic_reference.gd`)
            // must still harden, because `is_param_contravariant`'s hard-Variant-parent arm
            // (analyzer.cpp:1915-1918) is what makes the child signature mismatch and routes into
            // the `Cyclic reference.` emission. Without it the analyze ratchet drops to 195/196.
            let is_parameter = matches!(ctx.node(node_id).kind, NodeKind::Parameter(_));
            let soft_variant_stays_soft = !is_parameter
                && ty.kind == DtKind::Variant
                && ty.type_source == TypeSource::Inferred;
            ty.type_source = if (infer_datatype || is_constant) && !soft_variant_stays_soft {
                TypeSource::AnnotatedInferred
            } else {
                TypeSource::Inferred
            };
            // The stamp above is upstream's (analyzer.cpp:2163-2167), and it is what erases the
            // difference between `var un = v` and a gdls degrade. Carry the answer forward so a
            // member read off `un` still knows the value really is dynamic (#468). This is the
            // only site that softens an existing type, so it is the whole propagation surface.
            ty.dynamic_origin = ty.kind == DtKind::Variant && init_is_dynamic;
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
            if ctx.folds.is_constant(init) {
                crate::reducer::update_const_expression_builtin_type(
                    ctx, init, &ty, "assign", false,
                );
            }

            // analyzer.cpp:2162-2168 — `Cannot assign a value of type X to <kind> "Y" with
            // specified type Z.` when the initializer's narrowed type isn't compatible with the
            // declared specifier. Re-read after the literal/const-update narrowing above.
            let init_type = ctx.get_type(init).clone();
            // `!(!is_constant && reverse_compat)` => `is_constant || !reverse_compat` (de Morgan).
            let reverse_compat =
                !is_constant && crate::reducer::is_type_compatible(ctx, &init_type, &ty, false);
            // The forward check passes the initializer node upstream (analyzer.cpp:2158) — an
            // int initializer for an enum-typed declaration warns INT_AS_ENUM_WITHOUT_CAST.
            if init_type.is_hard_type()
                && !init_type.is_variant()
                && !crate::reducer::is_type_compatible_with_source(ctx, &ty, &init_type, true, init)
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

    // analyzer.cpp:2176-2191 (DEBUG_ENABLED) — UNTYPED_DECLARATION / INFERRED_DECLARATION on a
    // declaration with no `: Type` specifier. `:=` (or a constant, whose type is its value's) is
    // the inferred shape; a constant whose initializer is a metatype (a "type import" like
    // `const V2 = Vector2`) is exempt because there is no way to spell its true type.
    let is_parameter = matches!(ctx.node(node_id).kind, NodeKind::Parameter(_));
    if !has_specified_type {
        let declaration_type = if is_constant {
            "Constant"
        } else if is_parameter {
            "Parameter"
        } else {
            "Variable"
        };
        let infer_datatype = match &ctx.node(node_id).kind {
            NodeKind::Variable(v) => v.infer_datatype,
            NodeKind::Constant(c) => c.infer_datatype,
            NodeKind::Parameter(p) => p.infer_datatype,
            _ => false,
        };
        let name = decl_identifier_name(ctx, node_id);
        if infer_datatype || is_constant {
            let is_type_import =
                is_constant && initializer.is_some_and(|init| ctx.get_type(init).is_meta_type);
            if !is_type_import {
                ctx.push_warning(
                    crate::warnings::WarningCode::InferredDeclaration,
                    &[declaration_type.to_owned(), name],
                    node_id,
                );
            }
        } else {
            ctx.push_warning(
                crate::warnings::WarningCode::UntypedDeclaration,
                &[declaration_type.to_owned(), name],
                node_id,
            );
        }
    }

    // analyzer.cpp:2193-2204 (DEBUG_ENABLED) — ENUM_VARIABLE_WITHOUT_DEFAULT. Fires when a
    // variable (NOT a parameter or constant) has an explicit enum type, no initializer, and the
    // enum doesn't have a value of 0 (which would otherwise be the silent default). Godot's
    // `specified_type.kind == ENUM` reads the explicit annotation's resolved type; gdls's
    // equivalent is the same `ty` after the no-initializer path through `resolve_datatype`.
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
        // analyzer.cpp:1131-1135 — signal parameters don't go through `resolve_assignable`,
        // so the unannotated-parameter warning has its own site here.
        if spec.is_none() {
            ctx.push_warning(
                crate::warnings::WarningCode::UntypedDeclaration,
                &["Parameter".to_owned(), pname.clone()],
                param_id,
            );
        }
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
    // analyzer.cpp:1156-1157 — an element's custom value may name a sibling of the same block, so
    // `reduce_identifier`'s head arm needs to know which block is open. Saved and restored around
    // the loop (`prev_enum`) because a nested class member can be resolved from inside it.
    let prev_enum = ctx.current_enum.replace(enum_id);
    // `prev_value` matches Godot's `values[index-1].value` chain: -1 lets the first non-custom
    // entry fall into the `index == 0` branch (analyzer.cpp:1176) and resolve to 0.
    let mut prev_value: i64 = -1;
    for (ident_id, custom_value, ident_name) in entries {
        // Godot's `element.resolved`: true only when the value actually landed, so a *later*
        // sibling referring to a failed element still gets the forward-reference error.
        let mut resolved = true;
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
                    resolved = false;
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
                    resolved = false;
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
            // `element.resolved = true` (analyzer.cpp:1171/1179) — what makes a *later* sibling's
            // reference to this one a constant rather than the forward-reference error.
            if resolved {
                ctx.enum_element_values.insert(iid, value);
            }
            let mut value_dt = enum_type.clone();
            value_dt.is_meta_type = false;
            value_dt.builtin_type = VariantType::Int;
            value_dt.is_constant = true;
            ctx.set_type(iid, value_dt);
        }
        prev_value = value;
    }
    // analyzer.cpp:1193.
    ctx.current_enum = prev_enum;
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
pub(crate) fn make_class_enum_type(
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
            for v in &ne.values {
                t.enum_values
                    .insert(ctx.native.name_of(v.name).to_owned(), v.value);
            }
        }
    }
    t
}

/// Does builtin type `builtin` carry an enum named `name`? (analyzer.cpp:749's `has_enum`.)
pub(crate) fn builtin_has_enum(ctx: &AnalysisContext, builtin: VariantType, name: &str) -> bool {
    let base = crate::data_type::variant_type_name(builtin);
    ctx.native
        .builtin_named(base)
        .is_some_and(|bt| bt.enums.iter().any(|e| ctx.native.name_of(e.name) == name))
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
            for v in &ne.values {
                t.enum_values
                    .insert(ctx.native.name_of(v.name).to_owned(), v.value);
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
        for v in &ne.values {
            t.enum_values
                .insert(ctx.native.name_of(v.name).to_owned(), v.value);
        }
    } else if let Some(ne) = ctx.native.global_enum(enum_name) {
        // Some dumps key by the bare enum name.
        for v in &ne.values {
            t.enum_values
                .insert(ctx.native.name_of(v.name).to_owned(), v.value);
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

    // analyzer.cpp:1056-1061 then :1066-1107 — apply each member variable's annotations in
    // source order, then emit the two warnings that read the flags those applies set:
    // ONREADY_WITH_EXPORT (the variable ended up both `@onready` and `@export`ed) and
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
    for pending in lambdas {
        let func_id = match &ctx.node(pending.lambda_id).kind {
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
        let pre_lambda_stack = std::mem::take(&mut ctx.current_lambda_stack);
        ctx.concrete_function = pending.captured_concrete;
        ctx.static_context = pending.captured_static;
        ctx.suite_stack = pending.captured_suite_stack;
        ctx.current_lambda_stack = pending.captured_lambda_stack;
        ctx.push_current_lambda(pending.lambda_id);
        resolve_function_body(ctx, func_id, true);
        ctx.pop_current_lambda();
        ctx.concrete_function = pre_concrete;
        ctx.static_context = pre_static;
        ctx.suite_stack = pre_suite_stack;
        ctx.current_lambda_stack = pre_lambda_stack;
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
        // analyzer.cpp:624-627 — the resolve runs immediately before the apply, per annotation.
        resolve_annotation(ctx, ann_id);
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

/// Apply a function's annotations, in source order, during the interface pass
/// (gdscript_analyzer.cpp:1206-1209 applies each function's annotations before
/// `resolve_function_signature`). This is the function-level half of
/// `apply_abstract_annotations` plus `rpc_annotation` (gdscript_parser.cpp:5238); running them
/// at the right phase is what interleaves their errors with the class-level ones in Godot's
/// emission order.
fn apply_function_annotations(ctx: &mut AnalysisContext, fn_id: NodeId) {
    let fn_annotations: Vec<NodeId> = ctx.node(fn_id).annotations.clone();
    let mut fn_abstract_count = 0usize;
    let mut abstract_settled = false;
    let mut rpc_configured = false;
    for &ann_id in &fn_annotations {
        // analyzer.cpp:1206-1209 / :1412-1415 — resolve then apply, per annotation.
        resolve_annotation(ctx, ann_id);
        let ann_name = match &ctx.node(ann_id).kind {
            NodeKind::Annotation(an) => an.name.clone(),
            _ => continue,
        };
        if ann_name == "@rpc" {
            apply_rpc_annotation(ctx, ann_id, &mut rpc_configured);
            continue;
        }
        if ann_name != "@abstract" || abstract_settled {
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
            abstract_settled = true;
            continue;
        }
        if fn_abstract_count > 1 {
            ctx.push_error(
                r#""@abstract" annotation can only be used once per function."#,
                ann_id,
            );
            abstract_settled = true;
        }
    }
    if fn_abstract_count >= 1 {
        ctx.abstract_nodes.insert(fn_id);
    }
}

/// `GDScriptAnalyzer::resolve_annotation` (analyzer.cpp:1673-1727) — reduce each of an
/// annotation's arguments, fold it to a value, and check that value against the parameter the
/// registration declares. Idempotent: Godot's dispatcher reaches the same annotation from several
/// phases and only the first visit does the work (`AnnotationNode::is_resolved`).
///
/// The parameter index walks the registration but sticks on the last entry, which is what makes a
/// vararg annotation (`@export_flags("A", "B", "C")`) check every argument against its final
/// declared type (analyzer.cpp:1686-1689).
///
/// **Two departures, both under-reports.** Godot's `make_expression_reduced_value` materializes
/// every `Variant`; [`crate::FoldedValue`] cannot represent an array, a dictionary, a vector, a
/// math-utility result, or a preloaded resource. So the non-constant blame is gated on the
/// never-constant walk rather than on the fold's absence, and an argument gdls could not
/// materialize ends the walk without a message. A conversion gdls cannot perform (`StringName` to
/// `String`, say) does the same. Both truncate `resolved_arguments` exactly as an error does, so an
/// apply callback reading it can never see a value that was not checked.
pub(crate) fn resolve_annotation(ctx: &mut AnalysisContext, ann_id: NodeId) {
    use crate::data_type::{variant_can_convert_strict, variant_type_name, VariantType};
    use crate::FoldedValue;

    if !ctx.resolved_annotations.insert(ann_id) {
        return;
    }
    let (name, arg_ids) = match &ctx.node(ann_id).kind {
        NodeKind::Annotation(a) => (a.name.clone(), a.arguments.clone()),
        _ => return,
    };
    let Some(reg) = gd_syntax::parser::registered_annotation(&name) else {
        return; // An unregistered name — the parser already reported it.
    };
    // A zero-parameter annotation given arguments is the parser's arity error to report
    // (`Parser::validate_annotation_arguments`); there is nothing here to type them against.
    if reg.params.is_empty() {
        ctx.annotation_resolved_args.insert(ann_id, Vec::new());
        return;
    }

    let mut resolved: Vec<FoldedValue> = Vec::new();
    let mut param_index = 0usize;
    for (i, &arg_id) in arg_ids.iter().enumerate() {
        let want = match reg.params[param_index].ty {
            gd_syntax::parser::AnnotationParamType::Int => VariantType::Int,
            gd_syntax::parser::AnnotationParamType::Float => VariantType::Float,
            gd_syntax::parser::AnnotationParamType::String => VariantType::String,
        };
        if param_index + 1 < reg.params.len() {
            param_index += 1;
        }

        crate::reducer::reduce_expression(ctx, arg_id, false);
        // analyzer.cpp:1694 — the same fold pass the `const` initializer gets. #385.
        crate::reducer::fold_constant_site(ctx, arg_id);

        let Some(value) = ctx.folds.get(arg_id).cloned() else {
            // Godot gates this on `make_expression_reduced_value` having produced a value, which
            // it can always do for anything constant. gdls's fold table is narrower — `absi(-10)`
            // and `Vector3.UP` are both constant to Godot and unrepresentable here — so the blame
            // needs positive identification instead: the same never-constant walk a `const`
            // initializer uses. Anything the walk cannot place stays silent.
            if const_init_nonconstant_ref(ctx, arg_id).is_some() {
                ctx.push_error(
                    format!(
                        r#"Argument {} of annotation "{name}" isn't a constant expression."#,
                        i + 1
                    ),
                    arg_id,
                );
            }
            break;
        };

        let got = crate::reducer::folded_variant_type(&value);
        let value = if got == want {
            value
        } else {
            if want == VariantType::Int && got == VariantType::Float {
                ctx.push_warning(
                    crate::warnings::WarningCode::NarrowingConversion,
                    &[],
                    arg_id,
                );
            }
            if !variant_can_convert_strict(got, want) {
                let actual = ctx.get_type(arg_id).to_string();
                ctx.push_error(
                    format!(
                        r#"Invalid argument for annotation "{name}": argument {} should be "{}" but is "{actual}"."#,
                        i + 1,
                        variant_type_name(want)
                    ),
                    arg_id,
                );
                break;
            }
            let Some(converted) = convert_folded_value(&value, want) else {
                break;
            };
            converted
        };
        resolved.push(value);
    }
    ctx.annotation_resolved_args.insert(ann_id, resolved);
}

/// Resolve every annotation attached to `node_id`, in source order. Godot's member/statement loops
/// pair `resolve_annotation(E)` with `E->apply(...)`; where gdls has no apply for a kind (a
/// constant, a signal, an enum, a statement) this is the whole of that pairing.
fn resolve_node_annotations(ctx: &mut AnalysisContext, node_id: NodeId) {
    for ann_id in ctx.node(node_id).annotations.clone() {
        resolve_annotation(ctx, ann_id);
    }
}

/// `Variant::construct(p_type, …)` for the three parameter types an annotation registration can
/// declare, over the source types [`crate::FoldedValue`] represents. `None` where the conversion is
/// one Godot performs but gdls has no value for — the caller then truncates rather than inventing.
fn convert_folded_value(
    value: &crate::FoldedValue,
    want: crate::data_type::VariantType,
) -> Option<crate::FoldedValue> {
    use crate::data_type::VariantType;
    use crate::FoldedValue::{Bool, Float, Int};
    Some(match (want, value) {
        (VariantType::Int, Bool(b)) => Int(i64::from(*b)),
        (VariantType::Int, Float(f)) => Int(*f as i64),
        (VariantType::Float, Bool(b)) => Float(f64::from(*b)),
        (VariantType::Float, Int(n)) => Float(*n as f64),
        _ => return None,
    })
}

/// `rpc_annotation` (gdscript_parser.cpp:5238-5298) — one `@rpc` per function, and within it a
/// vocabulary check on each argument plus a "no more than once" check per config axis.
///
/// Reads `resolved_arguments` exactly as Godot does, so a `const MODE = "any_peer"` argument is
/// seen as the string it folded to. That list is short of the written arguments when
/// [`resolve_annotation`] stopped early, and the missing tail is simply not checked — a rejected
/// argument has already been reported once and must not be reported again as a bad RPC keyword.
///
/// `rpc_configured` is Godot's `function->rpc_config.get_type() != Variant::NIL`, which the apply
/// sets on its way out even when an argument was rejected — only the duplicate returns early.
/// Whether `name` is bound to `export_annotations` — the apply that builds a hint string out of its
/// arguments and therefore runs the value loop above.
///
/// `@export*` is not one family. `@export_storage`, `@export_custom`, and `@export_tool_button`
/// each register their OWN apply (`gdscript_parser.cpp:5008/5019/5047`) which reads its arguments
/// positionally and never renders them into a hint string, so none of the value checks apply to
/// them — `@export_custom(0, "")` is legal and means an empty hint string. The list is the
/// `export_annotations<...>` rows of the registration table (`gdscript_parser.cpp:152-173`),
/// transcribed in order; the grouping annotations below them are `STANDALONE` and never reach a
/// variable.
fn annotation_uses_export_hint_string(name: &str) -> bool {
    matches!(
        name,
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
    )
}

/// The `@export*` per-argument value loop (`gdscript_parser.cpp:4680-4740`). Returns `true` when it
/// reported — upstream returns from the whole apply on the first bad argument, so the caller skips
/// the rest of the export checks for that annotation.
///
/// The arguments are the ones [`resolve_annotation`] folded, which is short of what was written
/// when resolution stopped early; the missing tail is simply not checked, since a rejected argument
/// has already been reported once. That is the same rule [`apply_rpc_annotation`] follows.
///
/// Upstream renders each argument with `String(Variant)` and then asks whether it is empty or
/// contains a comma. Only a string-like value can be either: every other type an `@export*`
/// parameter accepts is a number, whose rendering is neither. So a non-string fold is skipped here
/// rather than rendered, which is the same answer without needing a `Variant::stringify` port —
/// and `resolve_annotation` has already rejected anything whose type does not fit the parameter.
fn apply_export_argument_values(ctx: &mut AnalysisContext, ann_id: NodeId, name: &str) -> bool {
    if !annotation_uses_export_hint_string(name) {
        return false;
    }
    let args: Vec<crate::FoldedValue> = ctx
        .annotation_resolved_args
        .get(&ann_id)
        .cloned()
        .unwrap_or_default();
    let arg_nodes = match &ctx.node(ann_id).kind {
        NodeKind::Annotation(a) => a.arguments.clone(),
        _ => return false,
    };
    for (i, value) in args.iter().enumerate() {
        // Each error anchors on its own ARGUMENT, not on the annotation.
        let anchor = arg_nodes.get(i).copied().unwrap_or(ann_id);
        let n = i + 1;
        let arg = match value {
            crate::FoldedValue::String(v)
            | crate::FoldedValue::StringName(v)
            | crate::FoldedValue::NodePath(v) => v.clone(),
            _ => continue,
        };
        // `@export_placeholder` is the one annotation whose argument may be anything at all — it
        // IS the placeholder text.
        if name != "@export_placeholder" {
            if arg.is_empty() {
                ctx.push_error(
                    format!(r#"Argument {n} of annotation "{name}" is empty."#),
                    anchor,
                );
                return true;
            }
            if arg.contains(',') {
                ctx.push_error(
                    format!(
                        r#"Argument {n} of annotation "{name}" contains a comma. Use separate arguments instead."#
                    ),
                    anchor,
                );
                return true;
            }
        }
        // cpp:4720-4731 — `@export_node_path`'s arguments name the classes a path may point at.
        if name == "@export_node_path" && apply_node_path_class_check(ctx, &arg, n, anchor) {
            return true;
        }
        // cpp:4694 — deliberately NOT an `else if`: `@export_flags` runs both checks.
        if name == "@export_flags" {
            const MAX_FLAGS: i64 = 32;
            let (flag_name, explicit_value) = match arg.split_once(':') {
                Some((n, v)) => (n, Some(v)),
                None => (arg.as_str(), None),
            };
            if flag_name.is_empty() {
                ctx.push_error(
                    format!(
                        r#"Invalid argument {n} of annotation "@export_flags": Expected flag name."#
                    ),
                    anchor,
                );
                return true;
            }
            match explicit_value {
                Some(v) => {
                    if v.is_empty() {
                        ctx.push_error(
                            format!(
                                r#"Invalid argument {n} of annotation "@export_flags": Expected flag value."#
                            ),
                            anchor,
                        );
                        return true;
                    }
                    let Some(parsed) = godot_valid_int(v) else {
                        ctx.push_error(
                            format!(
                                r#"Invalid argument {n} of annotation "@export_flags": The flag value must be a valid integer."#
                            ),
                            anchor,
                        );
                        return true;
                    };
                    if !(1..(1i64 << MAX_FLAGS)).contains(&parsed) {
                        ctx.push_error(
                            format!(
                                r#"Invalid argument {n} of annotation "@export_flags": The flag value must be at least 1 and at most 2 ** {MAX_FLAGS} - 1."#
                            ),
                            anchor,
                        );
                        return true;
                    }
                }
                None => {
                    if i as i64 >= MAX_FLAGS {
                        ctx.push_error(
                            format!(
                                r#"Invalid argument {n} of annotation "@export_flags": Starting from argument {}, the flag value must be specified explicitly."#,
                                MAX_FLAGS + 1
                            ),
                            anchor,
                        );
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// The `@export_node_path` leg of the argument loop (gdscript_parser.cpp:4720-4731): each argument
/// names a class that a path is allowed to point at, and Godot checks that the class exists, is
/// exposed, and inherits `Node`. Returns `true` when it reported — the caller then stops, matching
/// upstream's `return false` out of the whole apply.
///
/// A `class_name` argument resolves through the global-class registry to its native base
/// (`ScriptServer::get_global_class_native_base`), so `@export_node_path("MyNode2D")` is legal.
///
/// Two gates keep this from inventing errors, both on the "was not found" leg, which is the only
/// negative claim here:
///
/// * the API dump must be [`ApiProvenance::Exact`]. A generic dump proves what exists, never what
///   does not, and a project on a custom build legitimately names classes a stock dump lacks.
/// * a name the project registers as a `class_name` whose own base chain gdls could not walk to a
///   native root is unknown, not absent.
///
/// The "does not inherit Node" leg needs neither: it only fires on a class the DB does carry, and
/// engine ancestry does not move between builds.
fn apply_node_path_class_check(
    ctx: &mut AnalysisContext,
    arg: &str,
    n: usize,
    anchor: NodeId,
) -> bool {
    let global_file = ctx.xfile.global_class_file(arg);
    let native_class = match global_file {
        Some(file) => {
            let script_ref = ScriptRef {
                file,
                inner: Vec::new(),
            };
            match crate::script_chain::chain_native_root(ctx, &script_ref) {
                Some(root) => root,
                // fail-open: a registered global class whose chain gdls could not walk.
                None => return false,
            }
        }
        None => arg.to_string(),
    };

    // `ClassDB::class_exists` + `ClassDB::is_class_exposed`. The dump only carries exposed
    // classes, so carrying the name IS both conditions.
    if ctx.native.class_named(&native_class).is_none() {
        if ctx.native.provenance() != gd_types::ApiProvenance::Exact {
            return false;
        }
        // A global class always resolved to a native root above; if the DB does not carry that
        // root the gap is gdls's, not the user's.
        if global_file.is_some() {
            return false;
        }
        ctx.push_error(
            format!(
                r#"Invalid argument {n} of annotation "@export_node_path": The class "{arg}" was not found in the global scope."#
            ),
            anchor,
        );
        return true;
    }
    if !ctx.native.is_subclass_of_named(&native_class, "Node") {
        ctx.push_error(
            format!(
                r#"Invalid argument {n} of annotation "@export_node_path": The class "{arg}" does not inherit "Node"."#
            ),
            anchor,
        );
        return true;
    }
    false
}

/// `String::is_valid_int` + `String::to_int` (`core/string/ustring.cpp`): an optional leading `+`
/// or `-` followed by at least one digit and nothing else. Deliberately not Rust's `parse::<i64>`,
/// which accepts the same shapes but would also have to agree on overflow — Godot's `to_int` wraps
/// on an out-of-range literal where Rust's parse fails, and the caller's range check is what is
/// supposed to answer for a huge value.
fn godot_valid_int(text: &str) -> Option<i64> {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    // Saturating stands in for Godot's wrap: either way the value fails the 1..2**32 range check
    // below it, which is the only thing the result feeds.
    Some(text.parse::<i64>().unwrap_or(i64::MAX))
}

fn apply_rpc_annotation(ctx: &mut AnalysisContext, ann_id: NodeId, rpc_configured: &mut bool) {
    if *rpc_configured {
        ctx.push_error(
            "RPC annotations can only be used once per function.",
            ann_id,
        );
        return;
    }

    let args: Vec<crate::FoldedValue> = ctx
        .annotation_resolved_args
        .get(&ann_id)
        .cloned()
        .unwrap_or_default();
    let mut locality_args = 0u32;
    let mut permission_args = 0u32;
    let mut transfer_mode_args = 0u32;
    for (i, value) in args.iter().enumerate() {
        // cpp:5256 — the fourth argument is the transfer channel, never a mode keyword.
        if i == 3 {
            continue;
        }
        let crate::FoldedValue::String(arg) = value else {
            continue;
        };
        match arg.as_str() {
            "call_local" | "call_remote" => locality_args += 1,
            "any_peer" | "authority" => permission_args += 1,
            "reliable" | "unreliable" | "unreliable_ordered" => transfer_mode_args += 1,
            _ => ctx.push_error(
                r#"Invalid RPC argument. Must be one of: "call_local"/"call_remote" (local calls), "any_peer"/"authority" (permission), "reliable"/"unreliable"/"unreliable_ordered" (transfer mode)."#,
                ann_id,
            ),
        }
    }

    // cpp:5288-5294 — an else-if chain, so at most one of the three is reported.
    if locality_args > 1 {
        ctx.push_error(
            r#"Invalid RPC config. The locality ("call_local"/"call_remote") must be specified no more than once."#,
            ann_id,
        );
    } else if permission_args > 1 {
        ctx.push_error(
            r#"Invalid RPC config. The permission ("any_peer"/"authority") must be specified no more than once."#,
            ann_id,
        );
    } else if transfer_mode_args > 1 {
        ctx.push_error(
            r#"Invalid RPC config. The transfer mode ("reliable"/"unreliable"/"unreliable_ordered") must be specified no more than once."#,
            ann_id,
        );
    }

    *rpc_configured = true;
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
                // Anchor the warning at the variable's identifier (gdscript_analyzer.cpp:1444
                // anchors there too). The `@warning_ignore("…") var _b` span recorded by
                // `build_warning_ignored_lines` runs from the annotation through the declaration
                // header, which contains the identifier's line — suppression is by line, as
                // upstream.
                let at = match &ctx.node(var_id).kind {
                    NodeKind::Variable(v) => v.identifier.unwrap_or(var_id),
                    _ => var_id,
                };
                ctx.push_warning(WarningCode::UnusedPrivateClassVariable, &[name], at);
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
                ctx.push_warning(WarningCode::UnusedSignal, &[name], at);
            }
            _ => {}
        }
    }
}

/// Apply a class member variable's annotations in source order, then emit the two
/// `DEBUG_ENABLED` warnings that read the flags those applies set.
///
/// This mirrors gdscript_analyzer.cpp:1056-1061, which resolves and then applies every
/// annotation on a member in declaration order, followed by the warning block at :1066-1107.
/// Each apply is a gate chain that stops at the first failure and only sets its flag
/// (`onready`, `exported`) once it reaches the end, so a rejected annotation leaves the flag
/// clear and the next annotation of the same family fails the same way. Godot therefore reports
/// `@export @export_storage static var x` twice, once per apply.
///
/// The applies ported here are `onready_annotation` (gdscript_parser.cpp:4527) and the
/// `@export*` family's shared prologue (gdscript_parser.cpp:4660, repeated verbatim in
/// `export_storage_annotation` :4997, `export_custom_annotation` :5019 and
/// `export_tool_button_annotation` :5047), plus the two type-shape checks gdls can answer:
/// simple `@export` with nothing to infer from (:4792) and a Node-typed export outside a
/// Node-derived class (:4861 / :4932).
fn emit_variable_annotation_warnings(ctx: &mut AnalysisContext, class_id: NodeId) {
    use crate::warnings::WarningCode;

    // gdscript_parser.cpp:4530 / :4861 / :4932 — the node-ness of the enclosing class, read once.
    // `None` means the base chain is unknown, in which case gdls stays silent and lets the apply
    // through rather than inventing a rejection.
    // `is_tool()` (gdscript_parser.h) — the parser-wide flag `@tool` on the script's own head
    // sets. It is per SCRIPT, not per class, so an inner class reads the same value; and it is
    // read off the tree rather than the cross-file interface so a file analyzed on its own, with
    // no project behind it, still answers correctly.
    let script_is_tool = ctx.tree.root_id().is_some_and(|root| {
        ctx.tree.get(root).annotations.iter().any(
            |&a| matches!(&ctx.tree.get(a).kind, NodeKind::Annotation(an) if an.name == "@tool"),
        )
    });

    let native_base = nearest_native_ancestor(ctx, class_id);
    let is_node_derived = native_base
        .as_ref()
        .map(|base| ctx.native.is_subclass_of_named(base, "Node"));

    let total = member_count(ctx, class_id);
    for i in 0..total {
        let Some(Member::Variable(var_id)) = nth_member(ctx, class_id, i) else {
            continue;
        };
        let (annotations, is_static, has_type_specifier, initializer) = {
            let var_node = ctx.node(var_id);
            let annotations = var_node.annotations.clone();
            let (is_static, has_type_specifier, initializer) = match &var_node.kind {
                NodeKind::Variable(v) => {
                    (v.is_static, v.datatype_specifier.is_some(), v.initializer)
                }
                _ => (false, false, None),
            };
            (annotations, is_static, has_type_specifier, initializer)
        };

        let mut onready = false;
        let mut exported = false;

        for ann_id in annotations {
            let name = match &ctx.node(ann_id).kind {
                NodeKind::Annotation(a) => a.name.clone(),
                _ => continue,
            };
            // analyzer.cpp:1057-1062 — resolve then apply, per annotation.
            resolve_annotation(ctx, ann_id);
            if name == "@onready" {
                // gdscript_parser.cpp:4530-4544 — node-ness, then `static`, then the duplicate.
                if is_node_derived == Some(false) {
                    ctx.push_error(
                        r#""@onready" can only be used in classes that inherit "Node"."#,
                        ann_id,
                    );
                    continue;
                }
                if is_static {
                    ctx.push_error(
                        r#""@onready" annotation cannot be applied to a static variable."#,
                        ann_id,
                    );
                    continue;
                }
                if onready {
                    ctx.push_error(
                        r#""@onready" annotation can only be used once per variable."#,
                        ann_id,
                    );
                    continue;
                }
                onready = true;
            } else if name == "@export_tool_button" {
                // `export_tool_button_annotation` (gdscript_parser.cpp:5047-5091) is its own apply
                // with its own order: the tool-script check runs BEFORE the static and duplicate
                // checks every other `@export*` leads with, so a non-tool script with a static
                // tool button reports the tool error, not the static one.
                if !script_is_tool {
                    ctx.push_error(
                        r#"Tool buttons can only be used in tool scripts (add "@tool" to the top of the script)."#,
                        ann_id,
                    );
                    continue;
                }
                if is_static {
                    ctx.push_error(
                        format!(r#"Annotation "{name}" cannot be applied to a static variable."#),
                        ann_id,
                    );
                    continue;
                }
                if exported {
                    ctx.push_error(
                        format!(
                            r#"Annotation "{name}" cannot be used with another "@export" annotation."#
                        ),
                        ann_id,
                    );
                    continue;
                }
                // :5069-5074 — a hard, non-Variant type must be `Callable`. Both guards are
                // upstream's own, and together they are also the fail-open gate: a gdls degrade
                // yields either a `Variant` kind or a soft type, and neither reaches the check.
                let var_type = ctx.get_type(var_id).clone();
                if !var_type.is_variant()
                    && var_type.is_hard_type()
                    && (var_type.kind != DtKind::Builtin
                        || var_type.builtin_type != VariantType::Callable)
                {
                    ctx.push_error(
                        format!(
                            r#""@export_tool_button" annotation requires a variable of type "Callable", but type "{var_type}" was given instead."#
                        ),
                        ann_id,
                    );
                    continue;
                }
                // :5077 — set only once every check has passed, unlike `export_annotations`.
                exported = true;
            } else if name.starts_with("@export") {
                // gdscript_parser.cpp:4665-4674 — `static`, then a second `@export*` of any kind.
                if is_static {
                    ctx.push_error(
                        format!(r#"Annotation "{name}" cannot be applied to a static variable."#),
                        ann_id,
                    );
                    continue;
                }
                if exported {
                    ctx.push_error(
                        format!(
                            r#"Annotation "{name}" cannot be used with another "@export" annotation."#
                        ),
                        ann_id,
                    );
                    continue;
                }
                exported = true;

                // gdscript_parser.cpp:4680-4740 — the per-argument value loop. `exported` is
                // already set above, exactly as upstream sets it before this loop (:4674), so a
                // rejected argument still makes the NEXT `@export*` a duplicate.
                if apply_export_argument_values(ctx, ann_id, &name) {
                    continue;
                }

                // gdscript_parser.cpp:4792 — simple `@export` needs something to infer from.
                if name == "@export" && !has_type_specifier && initializer.is_none() {
                    ctx.push_error(
                        r#"Cannot use simple "@export" annotation with variable without type or initializer, since type can't be inferred."#,
                        ann_id,
                    );
                    continue;
                }

                // gdscript_parser.cpp:4744-4965 — the export TYPE checks, including the
                // Node-export-in-a-non-Node-class error. Only the annotations that route to
                // `export_annotations<...>` reach them; `@export_storage`, `@export_custom`, and
                // `@export_tool_button` register their own apply and check nothing here.
                if annotation_uses_export_hint_string(&name) {
                    apply_export_type_checks(
                        ctx,
                        ann_id,
                        &name,
                        var_id,
                        initializer,
                        is_node_derived,
                        native_base.as_deref(),
                    );
                }
            }
        }

        // analyzer.cpp:1067-1069 — both flags set → ONREADY_WITH_EXPORT.
        if onready && exported {
            ctx.push_warning(WarningCode::OnreadyWithExport, &[], var_id);
        }

        // analyzer.cpp:1070-1106 — non-static + non-onready + initializer is `$`/`%`/`get_node`.
        if !is_static && !onready {
            if let Some(init_id) = initializer {
                if let Some(offending) = get_node_default_form(ctx.tree, init_id) {
                    ctx.push_warning(
                        WarningCode::GetNodeDefaultWithoutOnready,
                        &[offending],
                        var_id,
                    );
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

/// #406: the claim-grade twin of [`nearest_native_ancestor`] — whether `class_id`'s whole ancestry
/// was walked end to end, so a name missing from all of it is genuinely missing.
///
/// The looser question ("where does this probably bottom out?") is what typing and the node-ness
/// gates read, and they must stay permissive when the answer is unknown. A negative claim cannot:
/// a link whose interface is unindexed, or one whose parse stopped at a syntax error and so
/// extracted a truncated member list, hides exactly the declaration the user wrote.
pub(crate) fn class_ancestry_introspectable(ctx: &AnalysisContext, class_id: NodeId) -> bool {
    let mut cur = Some(class_id);
    while let Some(c) = cur {
        let base = ctx.bases.get(&c).cloned().unwrap_or_default();
        match base.kind {
            DtKind::Native => return ctx.native.class_named(&base.native_type).is_some(),
            DtKind::Class => cur = base.class_node,
            DtKind::Script => {
                return base.script_type.as_ref().is_some_and(|sr| {
                    crate::script_chain::resolve_script_chain(ctx, sr).introspectable
                })
            }
            _ => return false,
        }
    }
    false
}

/// `_get_annotation_error_string` (gdscript_parser.cpp:4549-4599). Each expected builtin type
/// contributes its own name, its `Array[…]` form, and every packed array whose element type it is,
/// and the whole list is joined with an Oxford comma. Godot quotes each name with
/// `String::quote()`, so the message carries literal double quotes around every one of them.
fn export_annotation_error_string(
    annotation_name: &str,
    expected_types: &[VariantType],
    provided_type: &DataType,
) -> String {
    let mut types: Vec<String> = Vec::new();
    for &t in expected_types {
        let name = variant_type_name(t);
        types.push(name.to_string());
        types.push(format!("Array[{name}]"));
        // The switch at :4556-4578 — the packed arrays whose element type is `t`.
        match t {
            VariantType::Int => {
                types.push("PackedByteArray".into());
                types.push("PackedInt32Array".into());
                types.push("PackedInt64Array".into());
            }
            VariantType::Float => {
                types.push("PackedFloat32Array".into());
                types.push("PackedFloat64Array".into());
            }
            VariantType::String => types.push("PackedStringArray".into()),
            VariantType::Vector2 => types.push("PackedVector2Array".into()),
            VariantType::Vector3 => types.push("PackedVector3Array".into()),
            VariantType::Color => types.push("PackedColorArray".into()),
            VariantType::Vector4 => types.push("PackedVector4Array".into()),
            _ => {}
        }
    }

    let quoted: Vec<String> = types.iter().map(|t| format!("\"{t}\"")).collect();
    let list = match quoted.len() {
        0 => String::new(),
        1 => quoted[0].clone(),
        2 => format!("{} or {}", quoted[0], quoted[1]),
        n => format!("{}, or {}", quoted[..n - 1].join(", "), quoted[n - 1]),
    };

    format!(
        r#""{annotation_name}" annotation requires a variable of type {list}, but type "{provided_type}" was given instead."#
    )
}

/// The `t_type` template argument of each `export_annotations<hint, t_type>` registration
/// (gdscript_parser.cpp:152-173), which the tail check at :4967 compares the variable's type
/// against. Only ever read for a name [`annotation_uses_export_hint_string`] accepts, and `@export`
/// and `@export_enum` clear `use_default_variable_type_check` before the tail runs, so their `NIL`
/// is carried for parity and never compared.
fn export_annotation_expected_variant_type(name: &str) -> VariantType {
    match name {
        "@export_file"
        | "@export_file_path"
        | "@export_dir"
        | "@export_global_file"
        | "@export_global_dir"
        | "@export_multiline"
        | "@export_placeholder" => VariantType::String,
        "@export_range" | "@export_exp_easing" => VariantType::Float,
        "@export_color_no_alpha" => VariantType::Color,
        "@export_node_path" => VariantType::NodePath,
        "@export_flags"
        | "@export_flags_2d_render"
        | "@export_flags_2d_physics"
        | "@export_flags_2d_navigation"
        | "@export_flags_3d_render"
        | "@export_flags_3d_physics"
        | "@export_flags_3d_navigation"
        | "@export_flags_avoidance" => VariantType::Int,
        // "@export" and "@export_enum".
        _ => VariantType::Nil,
    }
}

/// What the object leg of the `@export` switch concluded (gdscript_parser.cpp:4808-4823).
enum ExportObjectLeg {
    /// A `Resource`-derived export, or an enum / builtin / Variant that never reaches ClassDB.
    Fine,
    /// A `Node`-derived export — the class it sits in must itself be Node-derived (:4861 / :4932).
    NodeExport,
    /// Neither, so upstream's "Export type can only be …" fired and the apply returned.
    Rejected,
    /// gdls could not see far enough to make the call. Upstream never has this case; here it means
    /// an unindexed base or a class absent from the native DB, and it must stay silent.
    Unknown,
}

/// The `case NATIVE / SCRIPT / CLASS` leg of the `@export` switch (gdscript_parser.cpp:4808-4823),
/// which upstream answers with two `ClassDB::is_parent_class` probes on `export_type.native_type`.
///
/// gdls resolves the native root of the type first, because a `Script`/`Class` kind carries a
/// project type whose native ancestor lives behind the cross-file chain. Two things upstream never
/// has to check gate the probes: a chain gdls could not walk to a native root, and a native name
/// the loaded dump does not carry. `is_subclass_of_named` answers `false` to BOTH probes for an
/// unknown name, which lands in the "Rejected" leg — so without the second gate an API dump that
/// is `Absent`, or merely missing a GDExtension class, would invent this error on ordinary code.
/// That also makes an explicit [`crate::native_db::ApiProvenance`] branch unnecessary: an absent
/// dump carries no classes at all, and a generic one only answers for real engine classes, whose
/// Resource/Node ancestry does not move between releases.
fn export_object_leg(ctx: &AnalysisContext, export_type: &DataType) -> ExportObjectLeg {
    let root = match export_type.kind {
        DtKind::Native => Some(export_type.native_type.clone()).filter(|s| !s.is_empty()),
        DtKind::Script => export_type
            .script_type
            .as_ref()
            .and_then(|sr| crate::script_chain::chain_native_root(ctx, sr)),
        DtKind::Class => export_type
            .class_node
            .and_then(|id| nearest_native_ancestor(ctx, id)),
        _ => None,
    };
    let Some(root) = root else {
        return ExportObjectLeg::Unknown;
    };
    if ctx.native.class_named(&root).is_none() {
        return ExportObjectLeg::Unknown;
    }
    if ctx.native.is_subclass_of_named(&root, "Resource") {
        ExportObjectLeg::Fine
    } else if ctx.native.is_subclass_of_named(&root, "Node") {
        ExportObjectLeg::NodeExport
    } else {
        ExportObjectLeg::Rejected
    }
}

/// One pass of the `@export` kind switch — the key leg at gdscript_parser.cpp:4802-4858 and, for a
/// typed `Dictionary`, the value leg at :4877-4926, which are the same switch written twice.
/// Returns the leg's verdict; the caller turns `Rejected` into the error and `NodeExport` into the
/// node-derived check.
///
/// The `ENUM` and `VARIANT` legs carry no diagnostic: everything they do upstream writes
/// `export_info`, which a language server has no consumer for. Upstream's `default:` leg (a
/// `RESOLVING`/`UNRESOLVED` kind) is the one place this port deliberately stays silent — in gdls
/// those two kinds are routinely a capability degrade rather than a broken program.
fn export_kind_leg(ctx: &AnalysisContext, export_type: &DataType) -> ExportObjectLeg {
    match export_type.kind {
        DtKind::Builtin | DtKind::Enum | DtKind::Variant => ExportObjectLeg::Fine,
        DtKind::Native | DtKind::Script | DtKind::Class => export_object_leg(ctx, export_type),
        // fail-open: upstream's `default:` error. See the doc comment above.
        DtKind::Resolving | DtKind::Unresolved => ExportObjectLeg::Unknown,
    }
}

/// The export TYPE checks — everything after the argument loop in `GDScriptParser::export_annotations`
/// (gdscript_parser.cpp:4742-4965).
///
/// Upstream's whole purpose there is to fill `variable->export_info` with a property type, a hint,
/// and a hint string. gdls has no consumer for any of that, so only the errors and the control flow
/// that reaches them are ported; every `export_info` write is dropped, and with it the `is_array`
/// re-wrap at :4975 and the dictionary key/value prefix strings.
///
/// The provided type printed in every message is the variable's OWN datatype
/// (`variable->get_datatype()`), not the element type extracted below — so `Array[RefCounted]`
/// reports as `"Array[RefCounted]"`.
#[allow(clippy::too_many_arguments)]
fn apply_export_type_checks(
    ctx: &mut AnalysisContext,
    ann_id: NodeId,
    name: &str,
    var_id: NodeId,
    initializer: Option<NodeId>,
    is_node_derived: Option<bool>,
    native_base: Option<&str>,
) {
    let provided = ctx.get_type(var_id).clone();
    let mut export_type = provided.clone();

    // :4745-4748 — a `Variant` declaration defers to the initializer's type.
    if export_type.is_variant() {
        if let Some(init_id) = initializer {
            let init_type = ctx.get_type(init_id).clone();
            if init_type.is_set() {
                export_type = init_type;
                export_type.type_source = TypeSource::Inferred;
            }
        }
    }

    // :4752-4760 — process the annotation on the ELEMENT type of an array or packed array.
    if export_type.kind == DtKind::Builtin
        && export_type.builtin_type == VariantType::Array
        && !export_type.container_element_types.is_empty()
    {
        export_type = export_type.container_element_types[0].clone();
    } else if export_type.is_typed_container_type() {
        let element = crate::data_type::typed_container_element(export_type.builtin_type)
            .expect("invariant: is_typed_container_type implies a packed-array element type");
        let source = export_type.type_source;
        export_type = DataType {
            kind: DtKind::Builtin,
            builtin_type: element,
            type_source: source,
            ..Default::default()
        };
    }

    // :4762-4768 — a typed `Dictionary` runs the switch twice, on the key and then on the value.
    // Upstream stashes the value type inside the key's own element slot to reuse one variable; a
    // named local is the mechanical equivalent.
    let mut dict_value_type: Option<DataType> = None;
    if export_type.kind == DtKind::Builtin
        && export_type.builtin_type == VariantType::Dictionary
        && !export_type.container_element_types.is_empty()
    {
        dict_value_type = Some(
            export_type
                .container_element_types
                .get(1)
                .cloned()
                .unwrap_or_else(DataType::variant),
        );
        export_type = export_type.container_element_types[0].clone();
    }

    let mut use_default_variable_type_check = true;

    if name == "@export_range" {
        // :4772-4776 — the INT special case only writes `export_info`.
    } else if name == "@export_multiline" {
        // :4777-4785
        use_default_variable_type_check = false;
        // fail-open: a type gdls itself could not pin down must not answer this check.
        if !export_type.is_positively_dynamic() {
            return;
        }
        if export_type.kind != DtKind::Builtin
            || !matches!(
                export_type.builtin_type,
                VariantType::String | VariantType::Dictionary
            )
        {
            ctx.push_error(
                export_annotation_error_string(
                    name,
                    &[VariantType::String, VariantType::Dictionary],
                    &provided,
                ),
                ann_id,
            );
            return;
        }
    } else if name == "@export" {
        // :4786-4940
        use_default_variable_type_check = false;

        // :4797-4800 — "the type of the initialized value can't be inferred". gdls cannot yet tell
        // its own inference gap from a genuine one here, so this error is not ported; the switch
        // below simply has nothing to say about an undetected type.
        if export_type.has_no_type() {
            return;
        }

        let mut node_export = match export_kind_leg(ctx, &export_type) {
            ExportObjectLeg::Rejected => {
                ctx.push_error(
                    "Export type can only be built-in, a resource, a node, or an enum.",
                    ann_id,
                );
                return;
            }
            ExportObjectLeg::Unknown => return,
            leg => matches!(leg, ExportObjectLeg::NodeExport),
        };

        // :4869-4941 — the value pass. Upstream rewrites a Variant or untyped value kind to
        // BUILTIN first (:4877-4879), which is exactly "no diagnostic".
        if let Some(value_type) = dict_value_type {
            if !(value_type.is_variant() || value_type.has_no_type()) {
                match export_kind_leg(ctx, &value_type) {
                    ExportObjectLeg::Rejected => {
                        ctx.push_error(
                            "Export type can only be built-in, a resource, a node, or an enum.",
                            ann_id,
                        );
                        return;
                    }
                    ExportObjectLeg::Unknown => return,
                    ExportObjectLeg::NodeExport => node_export = true,
                    ExportObjectLeg::Fine => node_export = false,
                }
            } else {
                node_export = false;
            }
        }

        // :4861 / :4932 — a Node-typed export needs a Node-derived class. `is_node_derived` is
        // `None` when the chain could not be walked, which stays silent. The base's string in the
        // template is `base_type.to_string()`, which for this chain walk is the bare native name.
        if node_export && is_node_derived == Some(false) {
            let base = native_base.unwrap_or_default();
            ctx.push_error(
                format!(
                    r#"Node export is only supported in Node-derived classes, but the current class inherits "{base}"."#
                ),
                ann_id,
            );
            return;
        }
    } else if name == "@export_enum" {
        // :4942-4955
        use_default_variable_type_check = false;
        let enum_type = if export_type.kind == DtKind::Builtin
            && export_type.builtin_type == VariantType::String
        {
            VariantType::String
        } else {
            VariantType::Int
        };
        // `is_variant()` covers `Resolving`/`Unresolved`, so an unresolved type excuses itself.
        if !export_type.is_variant()
            && (export_type.kind != DtKind::Builtin || export_type.builtin_type != enum_type)
        {
            ctx.push_error(
                export_annotation_error_string(
                    name,
                    &[VariantType::Int, VariantType::String],
                    &provided,
                ),
                ann_id,
            );
            return;
        }
    }

    // :4967-4977 — the default check against the registration's `t_type`, with the float/int
    // tolerance. No DB lookup happens here, and `is_variant()` again excuses an unresolved type.
    if use_default_variable_type_check {
        let t_type = export_annotation_expected_variant_type(name);
        if !export_type.is_variant()
            && (export_type.kind != DtKind::Builtin || export_type.builtin_type != t_type)
            && (t_type != VariantType::Float || export_type.builtin_type != VariantType::Int)
            && (t_type != VariantType::Int || export_type.builtin_type != VariantType::Float)
        {
            ctx.push_error(
                export_annotation_error_string(name, &[t_type], &provided),
                ann_id,
            );
        }
    }
}

/// Classify a class-variable initializer for the `GET_NODE_DEFAULT_WITHOUT_ONREADY` check
/// (analyzer.cpp:1073-1102). Returns `Some(offending_syntax)` if the initializer is `$Node` /
/// `%Unique` / a `get_node(...)` call (optionally wrapped in a single `Cast`). Returns `None`
/// otherwise — including for any expression shape Godot wouldn't flag.
///
/// Reads only node kinds, so it takes a [`ParseTree`] rather than a full [`AnalysisContext`]: this
/// is the SAME predicate the analyzer emits the warning from, exposed for the `gd_server` codeAction
/// drop-`@onready` quickfix to consult so it can REFUSE the fix when removing `@onready` would
/// re-induce this warning — reusing Godot's emission condition rather than re-deriving it (the
/// faithful-port discipline forbids replicating the predicate in two places, which would drift).
pub fn get_node_default_form(tree: &gd_syntax::ast::ParseTree, init_id: NodeId) -> Option<String> {
    // analyzer.cpp:1075-1077 — unwrap a single Cast wrapper.
    let inner_id = match &tree.get(init_id).kind {
        NodeKind::Cast(c) => c.operand?,
        _ => init_id,
    };

    match &tree.get(inner_id).kind {
        NodeKind::GetNode(gn) => Some(if gn.use_dollar { "$" } else { "%" }.to_owned()),
        NodeKind::Call(c) if c.function_name == "get_node" => {
            // analyzer.cpp:1083-1095 — only count when the callee is bare `get_node` or
            // `self.get_node` (an attribute subscript whose base is `self`). Other callees fall
            // through (Godot's switch-default).
            let callee_id = c.callee?;
            match &tree.get(callee_id).kind {
                NodeKind::Identifier(_) => Some("get_node()".to_owned()),
                NodeKind::Subscript(s) => match (s.access, s.base) {
                    (Some(gd_syntax::ast::SubscriptAccess::Attribute(_)), Some(base_id)) => {
                        if matches!(&tree.get(base_id).kind, NodeKind::SelfExpr) {
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

    // Decl-name-slot identifiers are excluded from the use-set so a `var _a` declaration
    // doesn't mark "_a" as referenced — Godot's `usages` counter is incremented in
    // `reduce_identifier` for true references, never on the decl identifier. The set is the
    // shared per-analysis cache on the context (built once, reused by every sweep).
    let decl_ident_ids = ctx.decl_ident_ids();

    // Collect names from identifier references (excluding the decl-name slots) and
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
    // identifier itself) and bare assignment targets (`parse_assignment` decrements the
    // parameter's `usages` on sight, gdscript_parser.cpp:3148-3150), via the shared
    // per-analysis caches on the context.
    let decl_ident_ids = ctx.decl_ident_ids();
    let assignee_ident_ids = ctx.assignee_ident_ids();
    let mut used_names = rustc_hash::FxHashSet::<String>::default();
    for id in ctx.tree.iter_ids() {
        let node = ctx.node(id);
        if decl_ident_ids.contains(&id) || assignee_ident_ids.contains(&id) {
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
    // Parse-time control-flow state, re-derived in resolve order. Godot computes `unreachable =
    // current_suite->has_return && !current_suite->has_unreachable_code` at the head of
    // `parse_statement` (gdscript_parser.cpp:2005) and latches + warns at its tail (:2205-2215);
    // gd_syntax keeps only the suite-final `has_return` flag, so the running value is rebuilt
    // here from the same three triggers (see `statement_guarantees_return`).
    let mut has_return = false;
    let mut has_unreachable_code = false;
    for stmt in stmts {
        // analyzer.cpp:2076-2080 — a statement's own annotations resolve before the statement.
        resolve_node_annotations(ctx, stmt);
        // gdscript_parser.cpp:2144-2190 — expression-statement shape warnings. Queued at parse
        // time in Godot, so on a shared line they precede both UNREACHABLE_CODE (queued at the
        // statement's parse tail) and any analyzer warning from inside the statement.
        //
        // `is_root` is what stands in for "this statement came out of `parse_statement`". The one
        // suite Godot resolves with `p_is_root = false` is a match-branch guard, and its single
        // statement is appended straight from `parse_expression(false)`
        // (gdscript_parser.cpp:2537) — the statement parser never sees it, so none of this family
        // can fire on a guard. (#460)
        if is_root {
            emit_standalone_statement_warnings(ctx, stmt);
        }
        if has_return && !has_unreachable_code {
            // The latch is unconditional; the warning needs an enclosing function (Godot skips
            // property setters/getters via its `if (current_function)` — same TODO as upstream).
            has_unreachable_code = true;
            if let Some(func_id) = ctx.current_function {
                let symbol = function_warning_name(ctx, func_id);
                ctx.push_warning(
                    crate::warnings::WarningCode::UnreachableCode,
                    &[symbol],
                    stmt,
                );
            }
        }
        resolve_node(ctx, stmt, is_root);
        // analyzer.cpp:2068 — drain pending lambda bodies after each statement so lambdas
        // queued by the just-resolved expression resolve in the right pass-relative order
        // (matters for the Godot-vs-gdls emission order on holding-function-with-lambdas cases).
        drain_pending_lambda_bodies(ctx);
        decide_suite_type(ctx, suite_id, stmt);
        has_return = has_return || statement_guarantees_return(ctx, stmt);
    }
    ctx.suite_stack.pop();
}

/// Godot's parse-time statement-shape warnings (gdscript_parser.cpp:2132-2160): an expression
/// used as a statement. `Assignment`/`Await`/`Call` are effectful; `preload` is function-like
/// but its result must be consumed (RETURN_VALUE_DISCARDED with symbol "preload"); a standalone
/// lambda is a parse *error* (emitted by `gd_syntax`); a `String` literal doubles as a multiline
/// comment; every other expression kind has no effect.
fn emit_standalone_statement_warnings(ctx: &mut AnalysisContext, stmt_id: NodeId) {
    use crate::warnings::WarningCode;
    let kind = ctx.node(stmt_id).kind.clone();
    match &kind {
        NodeKind::Assignment(_) | NodeKind::Await(_) | NodeKind::Call(_) => {}
        NodeKind::Preload(_) => {
            ctx.push_warning(
                WarningCode::ReturnValueDiscarded,
                &["preload".to_owned()],
                stmt_id,
            );
        }
        NodeKind::Lambda(_) => {} // `Standalone lambdas cannot be accessed` — gd_syntax error.
        // Godot exempts `Variant::STRING` only — a StringName or NodePath literal warns. Two
        // un-guarded arms, NOT one arm with a `!String` guard: a failed guard falls through to
        // the `is_expression()` catch-all below, which would warn for String literals too.
        NodeKind::Literal(gd_syntax::ast::LiteralNode {
            value: gd_syntax::token::Literal::String(_),
        }) => {}
        NodeKind::Literal(_) => {
            ctx.push_warning(WarningCode::StandaloneExpression, &[], stmt_id);
        }
        NodeKind::TernaryOp(_) => {
            ctx.push_warning(WarningCode::StandaloneTernary, &[], stmt_id);
        }
        k if k.is_expression() => {
            ctx.push_warning(WarningCode::StandaloneExpression, &[], stmt_id);
        }
        _ => {} // Statement kinds — not the expression-statement arm.
    }
}

/// The three parse-time `current_suite->has_return = true` triggers, re-derived per statement
/// (gd_syntax records only the suite-final flag): a `return` statement
/// (gdscript_parser.cpp:2078), an `if` whose both blocks return (:2383-2385), and a `match`
/// where every branch returns and a wildcard pattern exists (:2458-2460).
fn statement_guarantees_return(ctx: &AnalysisContext, stmt_id: NodeId) -> bool {
    let suite_returns = |b: Option<NodeId>| {
        b.is_some_and(|b| match &ctx.node(b).kind {
            NodeKind::Suite(s) => s.has_return,
            _ => false,
        })
    };
    match &ctx.node(stmt_id).kind {
        NodeKind::Return(_) => true,
        NodeKind::If(n) => suite_returns(n.true_block) && suite_returns(n.false_block),
        NodeKind::Match(n) => {
            let mut have_wildcard = false;
            let mut all_have_return = true;
            for &branch in &n.branches {
                if let NodeKind::MatchBranch(b) = &ctx.node(branch).kind {
                    have_wildcard = have_wildcard || b.has_wildcard;
                    all_have_return = all_have_return && suite_returns(b.block);
                }
            }
            all_have_return && have_wildcard
        }
        _ => false,
    }
}

/// The enclosing function's name for the UNREACHABLE_CODE symbol —
/// `current_function->identifier ? name : "<anonymous lambda>"` (gdscript_parser.cpp:2209).
fn function_warning_name(ctx: &AnalysisContext, func_id: NodeId) -> String {
    let ident = match &ctx.node(func_id).kind {
        NodeKind::Function(f) => f.identifier,
        _ => None,
    };
    ident
        .and_then(|id| match &ctx.node(id).kind {
            NodeKind::Identifier(i) => Some(i.name.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "<anonymous lambda>".to_owned())
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
        // analyzer.cpp:1609 takes `resolve_suite`'s default `true` here. gd_syntax's
        // `parse_statement` cannot yield a bare `Suite` statement, so this arm is defensive
        // either way — but `is_root` now carries the guard/not-guard distinction, so it has to
        // read the same as upstream.
        NodeKind::Suite(_) => resolve_suite(ctx, id, true),
        NodeKind::MatchBranch(_) => resolve_match_branch(ctx, id, None),
        NodeKind::Pattern(_) => resolve_match_pattern(ctx, id, None),
        NodeKind::Parameter(_) => resolve_parameter(ctx, id),
        NodeKind::Type(_) => {
            let _ = resolve_datatype(ctx, Some(id));
        }
        NodeKind::Annotation(_) => {
            // analyzer.cpp:1617-1619 — annotation `apply()` lands with WP-F.
            resolve_annotation(ctx, id);
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
    // UNUSED_VARIABLE (analyzer.cpp:2214-2218): `usages == 0` and not `_`-prefixed, anchored
    // at the declaration. Queued after resolve_assignable's own warnings, as upstream.
    warn_unused_local(ctx, var_id, crate::warnings::WarningCode::UnusedVariable);
    warn_local_shadowing(ctx, var_id, "variable");
    warn_confusable_identifier(ctx, var_id);
}

/// The `usages == 0 && !name.begins_with("_")` check shared by UNUSED_VARIABLE
/// (analyzer.cpp:2214-2218) and UNUSED_LOCAL_CONSTANT (:2228-2231). Godot's parser counts
/// `usages` by binding identifiers to locals at parse time; gdls has no parse-time identifier
/// resolution, so this sweeps identifier nodes between the declaration's end and the declaring
/// suite's end — over-approximating "used" exactly like `emit_unused_parameter_warnings` (a
/// same-named identifier that would bind to another scope still counts), so it can under-warn
/// but never false-positive. Declaration identifiers themselves are excluded, mirroring the
/// parameter sweep.
fn warn_unused_local(
    ctx: &mut AnalysisContext,
    decl_id: NodeId,
    code: crate::warnings::WarningCode,
) {
    let name = decl_identifier_name(ctx, decl_id);
    if name.is_empty() || name.starts_with('_') {
        return;
    }
    let Some(&suite_id) = ctx.suite_stack.last() else {
        return;
    };
    let suite_end = ctx.node(suite_id).span.end;
    let decl_end = ctx.node(decl_id).span.end;
    // Declaration identifiers never count as uses (Godot's `usages` counts references only),
    // and neither does a bare assignment target — `parse_assignment` decrements the local's
    // `usages` the moment it sees one (gdscript_parser.cpp:3141-3153). Both sets come from the
    // shared per-analysis caches on the context — one O(nodes) walk per analysis, not per
    // declaration.
    let decl_ident_ids = ctx.decl_ident_ids();
    let assignee_ident_ids = ctx.assignee_ident_ids();
    for id in ctx.tree.iter_ids() {
        let node = ctx.node(id);
        if node.span.start < decl_end || node.span.end > suite_end {
            continue;
        }
        if let NodeKind::Identifier(i) = &node.kind {
            if i.name == name && !decl_ident_ids.contains(&id) && !assignee_ident_ids.contains(&id)
            {
                return; // used
            }
        }
    }
    ctx.push_warning(code, std::slice::from_ref(&name), decl_id);
}

/// UNUSED_VARIABLE for a `match` pattern bind (analyzer.cpp:2494-2496). Same `usages == 0 &&
/// !name.begins_with("_")` test as `warn_unused_local`, but over the bind's own scope rather than
/// the enclosing suite: the parser registers the bind as a local in the branch's guard body and in
/// its block, and nowhere else (gdscript_parser.cpp:2521-2527 and :2560-2566), so every identifier
/// Godot's parser could resolve to it lies inside one of those two spans.
///
/// The sweep over-approximates "used" the same way its two siblings do — a same-named attribute
/// access or a same-named bind of a `match` nested inside the block also counts — so it can
/// under-warn but never false-positive. A bare assignment target is not a use
/// (gdscript_parser.cpp:3152, the `LOCAL_BIND` arm of the decrement), which is what
/// `assignee_ident_ids` carries.
fn warn_unused_pattern_bind(ctx: &mut AnalysisContext, bind_id: NodeId) {
    let name = decl_identifier_name(ctx, bind_id);
    if name.is_empty() || name.starts_with('_') {
        return;
    }
    let Some(branch_id) = ctx.current_match_branch else {
        return;
    };
    let (block, guard) = match &ctx.node(branch_id).kind {
        NodeKind::MatchBranch(n) => (n.block, n.guard_body),
        _ => return,
    };
    let spans: Vec<(usize, usize)> = [guard, block]
        .into_iter()
        .flatten()
        .map(|id| {
            let s = ctx.node(id).span;
            (s.start, s.end)
        })
        .collect();
    if spans.is_empty() {
        return;
    }
    let decl_ident_ids = ctx.decl_ident_ids();
    let assignee_ident_ids = ctx.assignee_ident_ids();
    for id in ctx.tree.iter_ids() {
        let node = ctx.node(id);
        if !spans
            .iter()
            .any(|&(start, end)| node.span.start >= start && node.span.end <= end)
        {
            continue;
        }
        if let NodeKind::Identifier(i) = &node.kind {
            if i.name == name && !decl_ident_ids.contains(&id) && !assignee_ident_ids.contains(&id)
            {
                return; // used
            }
        }
    }
    ctx.push_warning(
        crate::warnings::WarningCode::UnusedVariable,
        std::slice::from_ref(&name),
        bind_id,
    );
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

/// `TextServer::spoof_check` (`text_server_adv.cpp:7903-7928`) — ICU `uspoof_check` with the
/// allowed set `uspoof_getRecommendedSet() ∪ uspoof_getInclusionSet()` and restriction level
/// `USPOOF_MODERATELY_RESTRICTIVE`. `true` means the name is a spoofing risk.
///
/// UTS #39 §5.2 is what that restriction level is: a single-script identifier passes, and so does
/// Latin mixed with one of the CJK script sets; what fails is mixing Latin with Cyrillic or Greek,
/// and any character outside the allowed set. `check_restriction_level` answers both halves —
/// `detect_restriction_level` returns `Unrestricted` for a character whose Identifier_Status is not
/// Allowed, which is the same set ICU builds from those two calls.
///
/// Three documented under-reports, all in the safe direction. The crate's tables are Unicode 16
/// where the engine bundles ICU 78 (Unicode 17), so Bopomofo does not yet read as restricted; the
/// ICU `INVISIBLE` bit is not modelled, so a doubled combining mark (`á́b`) passes; and the check
/// runs on declarations and node names only, matching where gdls calls it rather than every
/// `parse_identifier` upstream covers.
///
/// A character the crate's tables do not know at all returns early as "not a spoof". That keeps a
/// future Unicode data bump from turning table-blindness into a false positive on ordinary code.
pub(crate) fn spoof_check(name: &str) -> bool {
    use unicode_security::restriction_level::{RestrictionLevel, RestrictionLevelDetection};
    use unicode_security::GeneralSecurityProfile;

    if name.is_ascii() {
        return false;
    }
    if name
        .chars()
        .any(|c| !c.is_ascii() && c.identifier_type().is_none())
    {
        return false;
    }
    !name.check_restriction_level(RestrictionLevel::ModeratelyRestrictive)
}

/// CONFUSABLE_IDENTIFIER (`gdscript_parser.cpp:2822-2825`) — Godot runs `TS->spoof_check` on every
/// identifier it parses as a declaration, and warns when it comes back true. #497.
fn warn_confusable_identifier(ctx: &mut AnalysisContext, node_id: NodeId) {
    let name = decl_identifier_name(ctx, node_id);
    if name.is_empty() {
        return;
    }
    if !spoof_check(&name) {
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
    // UNUSED_LOCAL_CONSTANT (analyzer.cpp:2227-2231) — the constant sibling of UNUSED_VARIABLE.
    warn_unused_local(
        ctx,
        const_id,
        crate::warnings::WarningCode::UnusedLocalConstant,
    );
    warn_confusable_identifier(ctx, const_id);
    // analyzer.cpp:2124-2133 — the constant initializer must reduce to a constant expression. See
    // `const_init_nonconstant_ref` for why this is a positive-identification walk over the AST
    // rather than a read of a fold bit.
    if let Some(init_id) = init {
        if const_init_nonconstant_ref(ctx, init_id).is_some() {
            let name = decl_identifier_name(ctx, const_id);
            // Anchored at the init expression, matching Godot's `p_assignable->initializer`.
            ctx.push_error(
                format!(r#"Assigned value for constant "{name}" isn't a constant expression."#),
                init_id,
            );
        }
    }
    warn_local_shadowing(ctx, const_id, "constant");
}

/// What a bare-identifier call can do inside a constant expression.
enum CallFold {
    /// Never constant: blame the call itself.
    Never,
    /// Could have folded, so an argument is what stopped it — walk them instead.
    Foldable,
    /// Never folds, and Godot never blames it either: only its arguments can disqualify it.
    ArgumentsOnly,
}

/// Classify a bare-identifier callee against Godot's own fold line, in `reduce_call`'s dispatch
/// order (analyzer.cpp:3248-3533):
///
/// - a builtin constructor folds when every argument is constant and the type is not shared
///   (:3288) — except `Array` and `Dictionary`, which `make_call_reduced_value` builds afterwards
///   (:5407-5450);
/// - a GDScript utility folds when its registration says it is constant (:3458);
/// - a Variant utility folds when it is in the `UTILITY_FUNC_TYPE_MATH` set (:3509);
/// - everything else — every project function, every engine method — never folds.
fn const_call_fold(ctx: &AnalysisContext, name: &str) -> CallFold {
    if matches!(name, "Array" | "Dictionary") {
        return CallFold::ArgumentsOnly;
    }
    if builtin_type_from_name(name).is_some() {
        // Reaching here means the constant fork did not fold it: a shared type it refuses outright
        // (`PackedByteArray()`), a signature no overload matches, or a non-constant argument. Godot
        // blames the constant in every one of those.
        return CallFold::Never;
    }
    let is_utility = ctx.native.utility(name).is_some()
        || gd_types::is_variant_utility(name)
        || crate::reducer::is_gdscript_utility(name);
    if is_utility {
        return if gd_types::is_variant_utility_math(name)
            || crate::reducer::is_gd_utility_constant(name)
        {
            CallFold::Foldable
        } else {
            CallFold::Never
        };
    }
    CallFold::Never
}

/// analyzer.cpp:2124-2133 — a constant initializer must reduce to a constant expression./// analyzer.cpp:2124-2133 — a constant initializer must reduce to a constant expression. Godot
/// decides that from `ExpressionNode::is_constant`, but only AFTER trying to force the value
/// through `make_expression_reduced_value`, which folds arrays, dictionaries, and constant calls.
/// gdls has no `make_*_reduced_value` family, so gating the error on the bit alone would reject
/// every `const A = [1, 2]`. The walk instead looks for a subexpression that can NEVER be
/// constant, whatever the fold table did or did not manage.
///
/// **Positive identification only.** Anything the walk cannot place — an unresolved name, a shape
/// outside the classification — is treated as constant and stays silent. A missed diagnostic is a
/// gap; a false one on a `const` Godot accepts would be a lie. A node the reducer already marked
/// constant short-circuits for the same reason: it is constant by construction.
///
/// Returns the offending node, `None` when nothing in the initializer disqualifies it.
fn const_init_nonconstant_ref(ctx: &AnalysisContext, expr_id: NodeId) -> Option<NodeId> {
    if ctx.folds.is_constant(expr_id) {
        return None;
    }
    let mut stack: Vec<NodeId> = vec![expr_id];
    while let Some(id) = stack.pop() {
        match &ctx.node(id).kind {
            NodeKind::Identifier(i) => {
                if matches!(classify_const_identifier(ctx, &i.name), ConstRef::Never) {
                    return Some(id);
                }
            }
            // analyzer.cpp:4789 — `reduce_self` sets `is_constant = false` unconditionally.
            NodeKind::SelfExpr => return Some(id),
            NodeKind::Call(c) => {
                // A call folds only through an IDENTIFIER callee: the builtin constructor
                // (analyzer.cpp:3326) and the two utility-function arms (:3493, :3544) are the only
                // sites that set `is_constant` on a call, and the `make_call_reduced_value` fallback
                // (:5391) opens with `if (p_call->get_callee_type() == IDENTIFIER)`. So an ATTRIBUTE
                // callee — `In.new()`, `Node.new()`, `Lib1.new()`, `obj.method()` — can never fold,
                // whatever it resolves to.
                //
                // An identifier callee is decided by the same three-way line Godot draws, in
                // `reduce_call`'s own dispatch order.
                let attribute_callee = c.callee.is_some_and(|callee| {
                    matches!(
                        &ctx.node(callee).kind,
                        NodeKind::Subscript(sub)
                            if matches!(
                                sub.access,
                                Some(gd_syntax::ast::SubscriptAccess::Attribute(_))
                            )
                    )
                });
                if attribute_callee {
                    return Some(id);
                }
                // The call folded, so it is constant by construction and its arguments cannot
                // disqualify it.
                if ctx.folds.is_constant(id) {
                    continue;
                }
                let Some(name) = c
                    .callee
                    .and_then(|callee| match &ctx.node(callee).kind {
                        NodeKind::Identifier(i) => Some(i.name.clone()),
                        _ => None,
                    })
                    .filter(|_| !c.is_super)
                else {
                    // A bare `super()` and a callee-less call are neither foldable nor
                    // classifiable; leave them alone rather than guess.
                    continue;
                };
                match const_call_fold(ctx, &name) {
                    // `Array(…)` / `Dictionary(…)` are the two names `make_call_reduced_value`
                    // rescues (analyzer.cpp:5407-5450): the constructor fork refuses them because
                    // they are shared types, and the fallback evaluator then builds them anyway.
                    // gdls has no value for either, so the CALL is never blamed and only its
                    // arguments are walked — `const A = Array([])` stays legal, as Godot has it.
                    CallFold::ArgumentsOnly => stack.extend(c.arguments.iter().copied()),
                    // A math or const-registered utility over constant arguments folds. It did
                    // not, so an argument is to blame — walk them and let one own the error.
                    CallFold::Foldable => stack.extend(c.arguments.iter().copied()),
                    // Every builtin constructor that reached here failed its constant fork, and
                    // every project function, engine method, and non-folding utility (`str`,
                    // `randi`, `range`, `load`, `print`) never had one to fail.
                    CallFold::Never => return Some(id),
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
            // `Base.member` — the ATTRIBUTE lookup is what makes a meta base constant
            // (analyzer.cpp:4817-4850), so `Node.PROCESS_MODE_INHERIT`, `Vector2.ZERO`, `Kind.ONE`,
            // and `External.InnerClass` all fold even though the base NAME alone would not. An
            // identifier base is therefore classified in SCOPE-ONLY mode: a local or member that is
            // not a constant still disqualifies it (`some_var.x`), while a global class / builtin /
            // global enum name does not. A non-identifier base (`self.x`, `[Node].foo`) carries no
            // such exemption and walks normally; a nested `A.B.C` re-enters this arm at the inner
            // subscript. The attribute identifier itself is never pushed.
            NodeKind::Subscript(s) => match s.access {
                Some(gd_syntax::ast::SubscriptAccess::Attribute(_)) => {
                    if let Some(base) = s.base {
                        match &ctx.node(base).kind {
                            NodeKind::Identifier(i) => {
                                if matches!(
                                    classify_const_identifier_in_scope(ctx, &i.name),
                                    ConstRef::Never
                                ) {
                                    return Some(base);
                                }
                            }
                            _ => stack.push(base),
                        }
                    }
                }
                Some(gd_syntax::ast::SubscriptAccess::Index(idx)) => {
                    if let Some(b) = s.base {
                        stack.push(b);
                    }
                    if let Some(idx) = idx {
                        stack.push(idx);
                    }
                }
                None => {
                    if let Some(b) = s.base {
                        stack.push(b);
                    }
                }
            },
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
            // Preload, Cast, TypeTest, Lambda, GetNode, Await, Literal — left constant-safe. The
            // first three genuinely fold in Godot; the rest never do, but they are outside what the
            // issue pinned against the oracle, so they stay an under-report rather than a guess.
            _ => {}
        }
    }
    None
}

/// What a bare name in a constant initializer resolves to, as far as constant-ness goes.
enum ConstRef {
    /// The name resolves to something that can never be a constant expression.
    Never,
    /// The name resolves to something that folds — no diagnostic, and stop looking.
    Fine,
    /// Nothing in this layer carries the name; resolution continues outward.
    Absent,
}

/// The SCOPE half of [`classify_const_identifier`]: locals, in-file class members, and the
/// cross-file base chain — everything that can SHADOW a global name. Split out because an attribute
/// base gets this half only (see the `Subscript` arm of [`const_init_nonconstant_ref`]).
fn classify_const_identifier_in_scope(ctx: &AnalysisContext, name: &str) -> ConstRef {
    use gd_syntax::ast::LocalKind;

    // A local. Only `LocalKind::Constant` folds (analyzer.cpp:4614-4655 — `LOCAL_BIND` sets
    // `is_constant` on the DataType, never on the node, so a pattern bind stays non-constant).
    if let Some(local) = crate::reducer::lookup_local(ctx, name) {
        return match local.kind {
            LocalKind::Constant => ConstRef::Fine,
            _ => ConstRef::Never,
        };
    }

    // An in-file class member, searched base-before-outer exactly as name resolution does.
    if let Some(class_id) = ctx.current_class {
        for look in scope_classes(ctx, class_id) {
            if class_identifier_name(ctx, look).as_deref() == Some(name) {
                // An in-file class NAME folds (analyzer.cpp:4040-4047).
                return ConstRef::Fine;
            }
            if let Some(member) = class_member(ctx, look, name) {
                return match member {
                    // analyzer.cpp:4205-4225 — CONSTANT / ENUM / ENUM_VALUE all fold, and
                    // MEMBER_CLASS folds through the helper at :4040-4047.
                    Member::Constant(_)
                    | Member::Enum(_)
                    | Member::EnumValue(_)
                    | Member::Class(_)
                    | Member::Group(_) => ConstRef::Fine,
                    Member::Variable(_) | Member::Signal(_) | Member::Function(_) => {
                        ConstRef::Never
                    }
                };
            }
        }
    }

    // The cross-file base chain. Without this step a base class's `const Node = 5` — or any
    // constant named like a native class — would fall through to the global arms and be reported,
    // which is exactly the false positive this walk must not produce.
    if let Some(base) = crate::reducer::current_class_script_base(ctx) {
        for link in crate::script_chain::scope_refs(ctx, &base) {
            let Some(iface) = crate::script_chain::link_interface(ctx.xfile, &link) else {
                continue;
            };
            if iface.enums.iter().any(|e| e.name.as_str() == name)
                || iface
                    .inner
                    .iter()
                    .any(|c| c.class_name.as_deref() == Some(name))
            {
                return ConstRef::Fine;
            }
            if let Some(m) = iface.members.iter().find(|m| m.name.as_str() == name) {
                return match m.kind {
                    gd_project::MemberKind::Const | gd_project::MemberKind::Enum => ConstRef::Fine,
                    gd_project::MemberKind::Var
                    | gd_project::MemberKind::Property
                    | gd_project::MemberKind::Func
                    | gd_project::MemberKind::Signal => ConstRef::Never,
                };
            }
        }
    }

    ConstRef::Absent
}

/// Whether a bare name used as a VALUE inside a constant initializer disqualifies it. Mirrors
/// `reduce_identifier`'s own precedence (analyzer.cpp:4387-4680): scope first, then the globals.
fn classify_const_identifier(ctx: &AnalysisContext, name: &str) -> ConstRef {
    match classify_const_identifier_in_scope(ctx, name) {
        found @ (ConstRef::Never | ConstRef::Fine) => return found,
        ConstRef::Absent => {}
    }

    // analyzer.cpp:4541-4545 — a native class name sets a meta DataType and no node `is_constant`.
    if ctx.native.class_named(name).is_some() {
        return ConstRef::Never;
    }
    // analyzer.cpp:4571-4574 — the `ScriptServer::is_global_class` arm, same story.
    if ctx.xfile.global_class_file(name).is_some() {
        return ConstRef::Never;
    }
    // analyzer.cpp:4556-4563 — a bare builtin type name. The "cannot be used as a name on its own"
    // and "not declared" errors fire first in Godot too; this is the third line it adds.
    if builtin_type_from_name(name).is_some() || name == "Variant" {
        return ConstRef::Never;
    }
    // analyzer.cpp:4620-4631 — a global CONSTANT (an enum's value included) folds …
    if ctx.native.global_enum_value(name).is_some() {
        return ConstRef::Fine;
    }
    // … while the global ENUM's own name (analyzer.cpp:4646-4652) does not.
    if ctx.native.global_enum(name).is_some() {
        return ConstRef::Never;
    }
    // Utility functions (`abs`), `PI`/`TAU`/`INF`/`NAN`, autoloads, inherited native members, and
    // anything unresolved: all fold in Godot, or already carry their own "not declared" error.
    ConstRef::Fine
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
        // No related location: the shadowed global is a builtin function / builtin type /
        // native class description, not a project declaration the analyzer can point at.
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
                let related = member_decl_ident_span(ctx, member_node)
                    .map(|span| {
                        vec![crate::diagnostic::RelatedInfo {
                            file: None,
                            span,
                            message: PREVIOUS_DECL_LABEL.to_owned(),
                        }]
                    })
                    .unwrap_or_default();
                ctx.push_warning_with_related(
                    crate::warnings::WarningCode::ShadowedVariable,
                    &[kind.to_owned(), name, member_kind, member_line],
                    ident_id,
                    related,
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
                    // The structured twin of `at line N in the base class "Y"`: the member's
                    // recorded name token in the declaring base file (zero-width only in
                    // defensively-built interfaces — skip rather than anchor junk).
                    let related = if member.name_span.is_empty() {
                        Vec::new()
                    } else {
                        vec![crate::diagnostic::RelatedInfo {
                            file: Some(link.file),
                            span: member.name_span,
                            message: PREVIOUS_DECL_LABEL.to_owned(),
                        }]
                    };
                    ctx.push_warning_with_related(
                        crate::warnings::WarningCode::ShadowedVariableBaseClass,
                        &[
                            kind.to_owned(),
                            name,
                            member_kind.to_owned(),
                            member_line,
                            base_name,
                        ],
                        ident_id,
                        related,
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
                // No related location: the shadowed declaration is a native method — its only
                // honest anchor would be a server-side API stub, which the analyzer cannot
                // materialize.
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
            // analyzer.cpp:2338 — the loop variable takes a Variant whose source stays at the
            // default UNDETECTED, which is what makes `var x := elem.member` an inference failure
            // one line later.
            //
            // This is the arm a gdls DEGRADE reaches: an unresolvable `preload` leaves the list a
            // soft `Variant`, and stamping UNDETECTED here would launder it into a type the rest
            // of the analyzer trusts, one loop body away from a false `Cannot infer …` (#468).
            // The other arms are reached only by a list gdls really did type.
            variable_type.kind = DtKind::Variant;
            if !list_type.is_positively_dynamic() {
                variable_type.type_source = TypeSource::Inferred;
            }
        } else if !list_type.container_element_types.is_empty() {
            // analyzer.cpp:2307-2309 — `has_container_element_type(0)`: the FIRST container
            // element type, which is `Array[T]`'s element AND `Dictionary[K, V]`'s KEY (iterating
            // a dictionary yields its keys). Ordered ahead of the builtin table exactly as
            // upstream, so it also beats the packed-array arm. The dictionary half was the
            // missing one: `for node in typed_dict` left the loop variable with no type at all,
            // which then poisoned every member access on it.
            variable_type = list_type.container_element_types[0].clone();
            variable_type.type_source = list_type.type_source;
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
                VariantType::Object | VariantType::Array | VariantType::Dictionary => {
                    // analyzer.cpp:2338 — an UNtyped array or dictionary genuinely yields
                    // `Variant`; the typed ones were taken by the container-element arm above.
                    // `Object`'s `_iter_get` walk (analyzer.cpp:2325-2337) needs the full method
                    // machinery and still degrades here.
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
        } else if let (DtKind::Class, Some(class_id)) = (list_type.kind, list_type.class_node) {
            // analyzer.cpp:2333-2345 — iterating an Object instance: look up the class's
            // `_iter_get(p_iter) -> T` method and use T as the iterator variable type. gdls
            // walks the in-file Class member directly; the cross-file Script + Native variants
            // join later slices.
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
            // The forward check passes `p_for->variable` upstream (analyzer.cpp:2335).
            if !specified_type.is_variant()
                && !variable_type.is_variant()
                && variable_type.is_hard_type()
                && !crate::reducer::is_type_compatible_with_source(
                    ctx,
                    &specified_type,
                    &variable_type,
                    true,
                    v,
                )
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
            ctx.set_type(v, variable_type.clone());
            // analyzer.cpp:2356-2362 — an unannotated iterator variable: a hard list-derived
            // type is an implicit inference; anything softer is plain untyped.
            let v_name = ident_name(ctx, v).unwrap_or_default();
            if variable_type.is_hard_type() {
                ctx.push_warning(
                    crate::warnings::WarningCode::InferredDeclaration,
                    &[r#""for" iterator variable"#.to_owned(), v_name],
                    v,
                );
            } else {
                ctx.push_warning(
                    crate::warnings::WarningCode::UntypedDeclaration,
                    &[r#""for" iterator variable"#.to_owned(), v_name],
                    v,
                );
            }
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
    // The function's `TypeTable` entry is its return type (see `resolve_function_signature`
    // analyzer.cpp:1729-1862 and gdls's mirror at the head of this file). A hard-typed
    // `Builtin NIL` return is Godot's `is_void_function`.
    let expected_type = ctx.current_function.map(|f| ctx.get_type(f).clone());
    let is_void_function = expected_type.as_ref().is_some_and(|t| {
        t.is_hard_type() && t.kind == DtKind::Builtin && t.builtin_type == VariantType::Nil
    });
    let Some(v) = value else {
        // analyzer.cpp:2534-2540 — a bare `return` yields a hard, constant `null`, and the compat
        // check below still runs against it. gdls used to bail here, which was invisible while an
        // untyped function's return type stayed a soft Variant.
        let nil = DataType {
            type_source: TypeSource::AnnotatedExplicit,
            kind: DtKind::Builtin,
            builtin_type: VariantType::Nil,
            is_constant: true,
            ..Default::default()
        };
        check_return_compatibility(ctx, ret_id, expected_type.clone(), &nil);
        ctx.set_type(ret_id, nil);
        return;
    };
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
    if expected_type.is_hard_type() && ctx.folds.is_constant(v) {
        crate::reducer::update_const_expression_builtin_type(
            ctx,
            v,
            &expected_type,
            "return",
            false,
        );
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
    check_return_compatibility(ctx, v, Some(expected_type), &result);

    // analyzer.cpp:2590 — stamp the return node's type so `decide_suite_type` can propagate it.
    ctx.set_type(ret_id, result);
}

/// analyzer.cpp:2578-2600 — the declared-return-type compatibility check, shared by the
/// value-returning and bare-`return` paths.
///
/// `anchor` is the node the diagnostic attaches to: the return *value* when there is one, the
/// `return` statement itself otherwise. See the emission-order note at the call site.
fn check_return_compatibility(
    ctx: &mut AnalysisContext,
    anchor: NodeId,
    expected_type: Option<DataType>,
    result: &DataType,
) {
    let Some(expected_type) = expected_type else {
        return;
    };
    if !expected_type.is_set() {
        return;
    }
    // DIALECT(4.7): gdscript_analyzer.cpp resolve_return() — the whole compat check gained
    // `expected_type.is_hard_type()`. A function whose return type was only *inferred* no longer
    // rejects a return value, since the inferred type is a guess about the body, not a contract
    // the user wrote.
    let expected_is_checkable = !expected_type.is_variant()
        && (ctx.dialect < Dialect::Godot4_7 || expected_type.is_hard_type());
    if expected_is_checkable && !result.is_variant() && result.is_hard_type() {
        // The forward check passes `p_return` upstream (analyzer.cpp:2572/2575); gdls anchors
        // at the return value (same line) per the emission-order note above.
        let target_to_source = crate::reducer::is_type_compatible_with_source(
            ctx,
            &expected_type,
            result,
            true,
            anchor,
        );
        if !target_to_source {
            let reverse = crate::reducer::is_type_compatible(ctx, result, &expected_type, false);
            if !reverse {
                ctx.push_error(
                    format!(
                        r#"Cannot return value of type "{result}" because the function return type is "{expected_type}"."#
                    ),
                    anchor,
                );
            }
        }
    }
}

/// `resolve_assert` (analyzer.cpp:2397): reduce the condition + message, then check the message's
/// type. The `ASSERT_ALWAYS_TRUE` / `ASSERT_ALWAYS_FALSE` warnings (analyzer.cpp:2396-2404) follow.
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
        // #556, analyzer.cpp:2400-2404 — the message must be a builtin `String`. Not
        // constant-gated: a `Variant`-typed expression fails because its KIND is not builtin, and a
        // `StringName` fails because only `String` passes.
        //
        // The `has_no_type` exemption is upstream's own, and it is what keeps this check honest
        // here for free: everything gdls could not resolve carries the no-type dummy, so a degrade
        // is skipped without a provenance gate of its own. The anchor is the message expression.
        let mt = ctx.get_type(m).clone();
        if !mt.has_no_type()
            && (mt.kind != DtKind::Builtin || mt.builtin_type != VariantType::String)
        {
            ctx.push_error("Expected string for assert error message.", m);
        }
    }
    // ASSERT_ALWAYS_TRUE / ASSERT_ALWAYS_FALSE (analyzer.cpp:2393-2399): a constant condition.
    // The FALSE arm skips a literal bool (`assert(false)` is a deliberate trap). An `Opaque`
    // fold's truthiness is unknown to gdls's value subset — skip rather than guess (Godot
    // booleanizes the materialized Variant; never lie).
    if let Some(c) = cond {
        if let Some(folded) = ctx.folds.get(c).cloned() {
            use crate::foldtable::FoldedValue;
            use crate::warnings::WarningCode;
            if !matches!(folded, FoldedValue::Opaque(..)) {
                if crate::reducer::booleanize(&folded) {
                    ctx.push_warning(WarningCode::AssertAlwaysTrue, &[], c);
                } else {
                    let is_bool_literal = matches!(
                        &ctx.node(c).kind,
                        NodeKind::Literal(l) if matches!(l.value, gd_syntax::token::Literal::Bool(_))
                    );
                    if !is_bool_literal {
                        ctx.push_warning(WarningCode::AssertAlwaysFalse, &[], c);
                    }
                }
            }
        }
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
    // UNREACHABLE_PATTERN (gdscript_parser.cpp:2433-2436): parse-time wildcard tracking — any
    // branch after one with a wildcard/bind-all pattern is unreachable, anchored at the
    // branch's first pattern. The check runs before the branch's own wildcard accumulates.
    let mut have_wildcard = false;
    for branch in branches {
        let (branch_has_wildcard, first_pattern) = match &ctx.node(branch).kind {
            NodeKind::MatchBranch(b) => (b.has_wildcard, b.patterns.first().copied()),
            _ => (false, None),
        };
        if have_wildcard {
            if let Some(p0) = first_pattern {
                ctx.push_warning(crate::warnings::WarningCode::UnreachablePattern, &[], p0);
            }
        }
        have_wildcard = have_wildcard || branch_has_wildcard;
        resolve_match_branch(ctx, branch, test);
    }
}

/// `resolve_match_branch` (analyzer.cpp:2417): resolve each pattern + the guard + the block.
fn resolve_match_branch(ctx: &mut AnalysisContext, branch_id: NodeId, match_test: Option<NodeId>) {
    let (patterns, block, guard) = match ctx.node(branch_id).kind.clone() {
        NodeKind::MatchBranch(n) => (n.patterns, n.block, n.guard_body),
        _ => return,
    };
    // analyzer.cpp:2433-2437 — the branch's own annotations first.
    resolve_node_annotations(ctx, branch_id);
    // A bind's scope is this branch's guard body and block, and nothing else. Saved and restored
    // so a `match` nested inside a branch block gives the outer branch back on the way out.
    let outer_branch = ctx.current_match_branch.replace(branch_id);
    for p in patterns {
        resolve_match_pattern(ctx, p, match_test);
    }
    ctx.current_match_branch = outer_branch;
    // analyzer.cpp:2443 — match-branch guard body uses explicit `false` (an expression context,
    // not a statement-root one). The block at :2446 uses the default `true`.
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
                if !ctx.folds.is_constant(e) {
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
                // analyzer.cpp:2492-2496 — shadow first, then unused, so the two land in that
                // order on a bind that is both.
                warn_local_shadowing(ctx, b, "pattern bind");
                warn_unused_pattern_bind(ctx, b);
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
                    // analyzer.cpp:2509-2512 — a dictionary pattern's key is matched by value, so
                    // it has to be known at analysis time. The test is Godot's `is_constant` bit,
                    // not whether the fold table holds a value (#364).
                    if !ctx.folds.is_constant(k) {
                        ctx.push_error(
                            "Expression in dictionary pattern key must be a constant.".to_owned(),
                            k,
                        );
                    }
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
    use gd_syntax::Dialect;
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
        WarnPolicy::build(
            &WarningConfig::default(),
            &StrictSettings::default(),
            Dialect::DEFAULT,
        )
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

    /// v1.0.2 (issue #24): the `Could not find type` negative claim requires `Exact` native
    /// provenance. Under `Absent` (no API source) or `Generic` (embedded stock fallback) the
    /// unknown name degrades to a silent Variant — a custom engine build's class is
    /// indistinguishable from a typo without the project's own surface.
    #[test]
    fn datatype_unknown_degrades_silently_without_exact_native_surface() {
        let generic = {
            let mut db = mini_native();
            db.set_provenance(gd_types::ApiProvenance::Generic);
            db
        };
        for (native, label) in [
            (gd_types::NativeDb::empty(), "Absent"),
            (generic, "Generic"),
        ] {
            let tree = gd_syntax::parse("extends Node\nvar g: Ghost\n").tree;
            let xfile = NoCrossFile;
            let pol = policy();
            let mut ctx =
                AnalysisContext::new(&tree, &native, &xfile, Some(FileId::new(1)), "", &pol);
            let _ = resolve_inheritance(&mut ctx);
            ctx.current_class = ctx.tree.root_id();
            let type_id = first_var_type(&tree);
            let dt = resolve_datatype(&mut ctx, Some(type_id));
            assert!(dt.is_variant(), "{label}: unknown type must degrade");
            assert!(
                !ctx.has_errors(),
                "{label}: no Could not find type error without Exact provenance"
            );
        }
    }

    /// The per-class carve-out to the Exact bar: a dump generated without extension
    /// registration (a failed DLL load silently unregisters the rest) is engine-`Exact` yet
    /// blind to classes Godot's own ClassDB carries. When the project declares the name via a
    /// GDExtension (recorded on the [`NativeDb`] by the server), "Could not find type" is
    /// exactly as unsound as the provenance cases above — degrade silently.
    #[test]
    fn datatype_of_an_extension_declared_class_degrades_silently_under_exact_provenance() {
        let mut native = mini_native();
        native.note_extension_declared_missing("Ghost");
        let tree = gd_syntax::parse("extends Node\nvar g: Ghost\n").tree;
        let xfile = NoCrossFile;
        let pol = policy();
        let mut ctx = AnalysisContext::new(&tree, &native, &xfile, Some(FileId::new(1)), "", &pol);
        let _ = resolve_inheritance(&mut ctx);
        ctx.current_class = ctx.tree.root_id();
        let type_id = first_var_type(&tree);
        let dt = resolve_datatype(&mut ctx, Some(type_id));
        assert!(dt.is_variant(), "extension-declared type must degrade");
        assert!(
            !ctx.has_errors(),
            "no Could not find type error for an extension-declared class"
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
