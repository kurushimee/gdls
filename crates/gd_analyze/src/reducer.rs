//! The `reduce_*` expression family — a faithful port of `GDScriptAnalyzer::reduce_expression`
//! (analyzer.cpp:2593) and the per-kind reducers it dispatches to. Mirrors Godot's
//! "visit-each-expression-exactly-once + assign a [`DataType`] + record a folded constant when we
//! can" contract, with the visited flag living in [`AnalysisContext::reduced`] and folded constants
//! in [`crate::foldtable::FoldTable`].
//!
//! WP-E lands this in vertical slices. **E1** (this file's first cut): the dispatcher, the
//! constant-folding evaluator over our `Nil|Bool|Int|Float|String` value subset, and the leaf
//! reducers — `reduce_literal` (analyzer.cpp:4687), `reduce_unary_op` (analyzer.cpp:5217),
//! `reduce_binary_op` (analyzer.cpp:3089), `reduce_array` (analyzer.cpp:2695), `reduce_dictionary`
//! (analyzer.cpp:3819), and `reduce_self` (analyzer.cpp:4759). Every other reducer is dispatched
//! to a stub that degrades the node to `Variant`, which is exactly the dispatcher's tail-guard
//! (analyzer.cpp:2686-2691): "if the kind handler left an `Unresolved` type, force it to `Variant`
//! so [`is_type_compatible`] never sees an unset target". `get_operation_type`/`is_type_compatible`
//! / `update_const_expression_builtin_type` land in E2/E3 alongside the matching reducers
//! ([`reduce_call`], [`reduce_subscript`], [`reduce_identifier`], [`reduce_assignment`]). Until
//! then, an un-fold-able binary/unary op typesafe-degrades to `Variant` — the project's "unknown
//! stays dynamic, never a phantom error" rule (`docs/00`).

use gd_syntax::ast::{BinaryOp, NodeId, NodeKind, UnaryOp};
use gd_syntax::token::Literal;

use crate::binding::{Binding, BindingTargetKind as BindingSymbolKind, CalleeTarget};
use crate::context::AnalysisContext;
use crate::data_type::{self, DataType, DtKind, TypeSource, VariantType};
use crate::foldtable::FoldedValue;
use crate::resolver::type_from_metatype;

/// Bare identifier of the enclosing concrete (non-lambda) function, for recording
/// `Binding::Call.caller_function` (it is the raw `i.name`, never class-qualified — see that
/// field's doc). Returns `None` outside any function (e.g. a class-level member initializer) or
/// when the FunctionNode's identifier is missing (rare lambda case).
fn caller_function_name(ctx: &AnalysisContext) -> Option<String> {
    let fid = ctx.concrete_function?;
    let f = match &ctx.node(fid).kind {
        NodeKind::Function(f) => f,
        _ => return None,
    };
    let ident_id = f.identifier?;
    match &ctx.node(ident_id).kind {
        NodeKind::Identifier(i) => Some(i.name.clone()),
        _ => None,
    }
}

/// The inner-class name chain from the file's root class down to `class_id` (empty = the root
/// class itself; [`crate::data_type::ScriptRef::inner`]'s vocabulary) — the owning-class path
/// recorded on [`crate::binding::CalleeTarget::Script`]. Walks the class-member tree from the
/// root (the flat arena has no parent pointers): O(classes) per recorded call, noise next to
/// the resolution that preceded it.
fn class_inner_path(ctx: &AnalysisContext, class_id: NodeId) -> Vec<String> {
    fn walk(
        ctx: &AnalysisContext,
        current: NodeId,
        target: NodeId,
        path: &mut Vec<String>,
    ) -> bool {
        if current == target {
            return true;
        }
        let NodeKind::Class(c) = &ctx.node(current).kind else {
            return false;
        };
        for m in &c.members {
            let gd_syntax::ast::Member::Class(inner_id) = m else {
                continue;
            };
            let name = match &ctx.node(*inner_id).kind {
                NodeKind::Class(ic) => ic
                    .identifier
                    .and_then(|iid| match &ctx.node(iid).kind {
                        NodeKind::Identifier(i) => Some(i.name.clone()),
                        _ => None,
                    })
                    .unwrap_or_default(),
                _ => String::new(),
            };
            path.push(name);
            if walk(ctx, *inner_id, target, path) {
                return true;
            }
            path.pop();
        }
        false
    }
    let mut path = Vec::new();
    if let Some(root) = ctx.tree.root_id() {
        if walk(ctx, root, class_id, &mut path) {
            return path;
        }
    }
    // Defensive: a class node unreachable from the root (recovered parse) records the root
    // path rather than a fabricated one.
    Vec::new()
}

// ===================================================================================================
// reduce_expression — analyzer.cpp:2593
// ===================================================================================================

/// `GDScriptAnalyzer::reduce_expression(p_expression, p_is_root)` (analyzer.cpp:2593): visit an
/// expression node once, dispatching to its kind-specific reducer; on entry mark the node visited so
/// a recursive cycle stops, on exit guarantee the node has a determined type (Godot's tail-guard
/// at analyzer.cpp:2686). `is_root` is propagated to the call/ternary reducers in E2.
pub(crate) fn reduce_expression(ctx: &mut AnalysisContext, id: NodeId, is_root: bool) {
    if !ctx.reduced.insert(id) {
        // Already reduced this node — don't do this more than once (analyzer.cpp:2600).
        return;
    }
    // M5 WP-O3 / WP-O4 governor + cancellation checkpoint. Placed AFTER the visited-guard so
    // only the once-per-node path counts toward the budget (a re-entry that hit the cache
    // above is free). Bail leaves the partial result intact — the sink already holds whatever
    // diagnostics fired before the budget tripped.
    let span = ctx.tree.get(id).span;
    if ctx.checkpoint(span) {
        return;
    }

    let is_expr = ctx.node(id).kind.is_expression();
    if !is_expr {
        // Godot hits `ERR_FAIL_MSG("Reaching unreachable case")` for non-expressions
        // (analyzer.cpp:2683). gdls never crashes on a malformed tree (CLAUDE.md "never crash, never
        // lie") — degrade silently to `Variant`, the same shape the tail-guard below produces.
        ctx.set_type(id, variant_dt());
        return;
    }

    // E1 ports the leaf reducers; E2 wires `reduce_expression` into the body driver. For the
    // expression kinds whose full reducer hasn't landed yet (call/subscript/identifier/cast/
    // ternary/assignment/await/lambda/preload/get_node/type_test), we need to **recurse into the
    // child expressions** here so a binary/unary op nested inside (`print(2.2 << 4)`,
    // `arr[i + 1]`, `foo(a * b, c)`, …) still reaches its constant-fold path. The wildcard `_ =>
    // ()` of the earlier E1 cut bypassed the children — exactly why `errors/bitwise_float_*`
    // never produced their `Invalid operands` diagnostic before this slice. The stub reducers
    // recurse-and-degrade; the final reducers (E3-onwards) override these arms.
    match ctx.node(id).kind.clone() {
        NodeKind::Literal(_) => reduce_literal(ctx, id),
        NodeKind::UnaryOp(_) => reduce_unary_op(ctx, id),
        NodeKind::BinaryOp(_) => reduce_binary_op(ctx, id),
        NodeKind::Array(_) => reduce_array(ctx, id),
        NodeKind::Dictionary(_) => reduce_dictionary(ctx, id),
        NodeKind::SelfExpr => reduce_self(ctx, id),

        NodeKind::Call(c) => {
            // analyzer.cpp:3300-3320 — for an attribute-style callee (`x.method()`), reduce only
            // the *base* of the subscript, not the subscript itself; the method lookup is
            // performed by reduce_call against `base_type` (which can see Dictionary methods on
            // enum metas, builtin methods, static methods, etc.). The dispatcher reduces the
            // callee's base here so reduce_call can read `base_type` directly; reduce_call
            // performs the rest of the lookup + the diagnostics.
            if let Some(callee) = c.callee {
                match &ctx.node(callee).kind {
                    NodeKind::Subscript(s)
                        if matches!(
                            s.access,
                            Some(gd_syntax::ast::SubscriptAccess::Attribute(_))
                        ) =>
                    {
                        if let Some(base) = s.base {
                            reduce_expression(ctx, base, false);
                        }
                    }
                    _ => {
                        // analyzer.cpp:3380+ — when an identifier is reduced as a *call target*,
                        // Godot resolves it via `reduce_identifier_from_base` (the
                        // with-base variant), which does **not** run the
                        // `static_context && instance-member` access check at
                        // analyzer.cpp:4464-4490. gdls's dispatcher pre-reduces via
                        // `reduce_identifier` (the without-base variant), which does. Mark
                        // the callee-reduction so `reduce_identifier` can skip the access-
                        // check; the call-version of the static-context check
                        // (reducer.rs:2322) is the one that fires for calls.
                        let prev = ctx.reducing_callee;
                        ctx.reducing_callee = true;
                        reduce_expression(ctx, callee, false);
                        ctx.reducing_callee = prev;
                    }
                }
            }
            reduce_call(ctx, id, is_root);
        }
        NodeKind::Subscript(_) => reduce_subscript(ctx, id, false),
        NodeKind::TernaryOp(t) => {
            if let Some(c) = t.condition {
                reduce_expression(ctx, c, false);
            }
            if let Some(e) = t.true_expr {
                reduce_expression(ctx, e, false);
            }
            if let Some(e) = t.false_expr {
                reduce_expression(ctx, e, false);
            }
            // analyzer.cpp:5172-5186 ternary result typing for same-shaped branches. Both
            // branches HARD ⇒ the common type at ANNOTATED_INFERRED (Godot's
            // `true_type.is_hard_type() && false_type.is_hard_type() ? ANNOTATED_INFERRED :
            // INFERRED` tail) — `x := a if c else Color.WHITE` with two hard Colors must infer
            // Color, not degrade. Mixed hard/soft keeps the Undetected result the corpus's
            // `ternary_weak_infer.gd` pins (gdls's resolve_assignable "Cannot infer" contract).
            // Differing shapes fall to the dispatcher tail-guard's soft Variant.
            if let (Some(te), Some(fe)) = (t.true_expr, t.false_expr) {
                let tt = ctx.get_type(te).clone();
                let ft = ctx.get_type(fe).clone();
                // INCOMPATIBLE_TERNARY (analyzer.cpp:5172-5184): neither branch type accepts
                // the other — the values have no common type and the expression degrades to
                // Variant. Variant-typed (or unset, tail-guard-pending) branches are exempt,
                // exactly as upstream's `is_variant()` early arm.
                if tt.is_set()
                    && ft.is_set()
                    && tt.kind != DtKind::Variant
                    && ft.kind != DtKind::Variant
                    && !is_type_compatible(ctx, &tt, &ft, false)
                    && !is_type_compatible(ctx, &ft, &tt, false)
                {
                    ctx.push_warning(crate::warnings::WarningCode::IncompatibleTernary, &[], id);
                }
                if tt.is_set()
                    && ft.is_set()
                    && tt.kind == ft.kind
                    && tt.builtin_type == ft.builtin_type
                {
                    if tt.is_hard_type() && ft.is_hard_type() {
                        let mut result = tt.clone();
                        result.type_source = TypeSource::AnnotatedInferred;
                        result.is_constant = false;
                        ctx.set_type(id, result);
                    } else if tt.type_source != ft.type_source {
                        ctx.set_type(
                            id,
                            DataType {
                                type_source: TypeSource::Undetected,
                                kind: tt.kind,
                                builtin_type: tt.builtin_type,
                                ..Default::default()
                            },
                        );
                    }
                }
            }
        }
        NodeKind::Cast(_) => reduce_cast(ctx, id),
        NodeKind::Assignment(_) => reduce_assignment(ctx, id),
        NodeKind::Await(_) => reduce_await(ctx, id),
        NodeKind::TypeTest(_) => reduce_type_test(ctx, id),
        NodeKind::Preload(_) => reduce_preload(ctx, id),
        NodeKind::Identifier(_) => reduce_identifier(ctx, id),
        NodeKind::GetNode(_) => reduce_get_node(ctx, id),
        NodeKind::Lambda(_) => reduce_lambda(ctx, id),

        // Non-expression kinds — Godot's `ERR_FAIL_MSG("Reaching unreachable case")` arm. The
        // earlier `is_expression()` guard catches these; this branch keeps the match exhaustive.
        _ => {
            let _ = is_root;
        }
    }

    // Tail-guard (analyzer.cpp:2686-2691): prevent `is_type_compatible()` errors for incomplete
    // expressions by promoting an unset type to `Variant`.
    if ctx.get_type(id).kind == DtKind::Unresolved {
        ctx.set_type(id, variant_dt());
    }
}

/// A bare `Variant` `DataType` — the dispatcher tail-guard's default and Godot's catch-all when
/// an expression's type cannot be narrowed. Godot's tail-guard at analyzer.cpp:2686-2691
/// default-constructs `DataType` (whose `type_source` is `UNDETECTED`); gdls **deliberately uses
/// `Inferred`** because our partial reducer hits this fallback far more often than Godot's
/// complete one does — and a `UNDETECTED`-sourced Variant would false-positive Godot's
/// `Cannot infer the type of "X" … because the value doesn't have a set type.` error on every
/// `var x := <un-ported-reducer expression>` in the corpus. Once `reduce_identifier`/
/// `reduce_call`/`reduce_subscript`/… all produce determined types this divergence is moot.
fn variant_dt() -> DataType {
    DataType {
        kind: DtKind::Variant,
        type_source: TypeSource::Inferred,
        ..Default::default()
    }
}

// ===================================================================================================
// reduce_literal — analyzer.cpp:4687
// ===================================================================================================

/// `GDScriptAnalyzer::reduce_literal` (analyzer.cpp:4687): record the literal as a constant + derive
/// its type via [`type_from_variant`] over the literal's value.
fn reduce_literal(ctx: &mut AnalysisContext, id: NodeId) {
    let NodeKind::Literal(lit) = &ctx.node(id).kind else {
        return;
    };
    let folded = folded_from_literal(&lit.value);
    let dt = type_from_variant(&folded);
    ctx.folds.set(id, folded);
    ctx.set_type(id, dt);
}

/// Map a `gd_syntax::token::Literal` (the parser-emitted constant value) to our `FoldedValue` —
/// gdls's `Nil|Bool|Int|Float|String` subset of `Variant`. `StringName`/`NodePath` are stored as
/// `String` for folding purposes (their builtin `VariantType` is still recorded on the literal's
/// type via [`type_from_variant`] below — except for these two, where the AST conflates them with
/// strings; E3 distinguishes them when the proper reducer for them lands).
fn folded_from_literal(lit: &Literal) -> FoldedValue {
    match lit {
        Literal::Int(v) => FoldedValue::Int(*v),
        Literal::Float(v) => FoldedValue::Float(*v),
        Literal::String(s) => FoldedValue::String(s.clone()),
        Literal::StringName(s) => FoldedValue::String(s.clone()),
        Literal::NodePath(s) => FoldedValue::String(s.clone()),
        Literal::Bool(b) => FoldedValue::Bool(*b),
        Literal::Null => FoldedValue::Nil,
    }
}

// ===================================================================================================
// type_from_variant — analyzer.cpp:5701
// ===================================================================================================

/// `GDScriptAnalyzer::type_from_variant(p_value, p_source)` (analyzer.cpp:5701) over our
/// `FoldedValue` subset. Folded values are by construction one of `Nil`/`Bool`/`Int`/`Float`/`String`
/// — no `Array`/`Dictionary`/`Object` to consider yet — so this is the prefix of Godot's algorithm
/// before the `ARRAY`/`DICTIONARY`/`OBJECT` arms. Marks the result `is_constant = true` and source
/// `AnnotatedExplicit` per Godot ("Constant has explicit type", analyzer.cpp:5706).
pub fn type_from_variant(value: &FoldedValue) -> DataType {
    DataType {
        kind: DtKind::Builtin,
        type_source: TypeSource::AnnotatedExplicit,
        is_constant: true,
        builtin_type: match value {
            FoldedValue::Nil => VariantType::Nil,
            FoldedValue::Bool(_) => VariantType::Bool,
            FoldedValue::Int(_) => VariantType::Int,
            FoldedValue::Float(_) => VariantType::Float,
            FoldedValue::String(_) => VariantType::String,
            FoldedValue::Opaque(vt) => *vt,
        },
        ..Default::default()
    }
}

// ===================================================================================================
// reduce_unary_op — analyzer.cpp:5217
// ===================================================================================================

/// `GDScriptAnalyzer::reduce_unary_op` (analyzer.cpp:5217). E1 reproduces the constant-fold path
/// (analyzer.cpp:5230-5234) for the operators that act on our `Int|Float|Bool` folded values;
/// `get_operation_type` (analyzer.cpp:5241) — the type-only path for non-constant operands — lands
/// in E2 with the operator type matrix. Until then, a non-constant operand types as `Variant`
/// (Godot's variant-operand arm at analyzer.cpp:5236-5238 made unconditional — safe, the operator-
/// validity error returns in E2/E3).
fn reduce_unary_op(ctx: &mut AnalysisContext, id: NodeId) {
    let NodeKind::UnaryOp(op_node) = ctx.node(id).kind.clone() else {
        return;
    };
    let Some(operand_id) = op_node.operand else {
        ctx.set_type(id, variant_dt());
        return;
    };
    reduce_expression(ctx, operand_id, false);

    if ctx.folds.is_reduced(operand_id) {
        let operand = ctx.folds.get(operand_id).cloned();
        if let Some(folded) = operand.and_then(|v| eval_unary(op_node.operation, &v)) {
            let dt = type_from_variant(&folded);
            ctx.folds.set(id, folded);
            ctx.set_type(id, dt);
            return;
        }
    }
    ctx.set_type(id, variant_dt());
}

/// Constant-fold a unary operation over our `FoldedValue` subset, mirroring `Variant::evaluate` for
/// the unary cases. Returns `None` when the (operator, operand-kind) pair has no defined
/// `Variant::evaluate` — the analog of `r_valid = false`. Godot's per-type registrations are at
/// `core/variant/variant_op.cpp`:
/// * `OP_NEGATE` / `OP_POSITIVE` / `OP_BIT_NEGATE` are registered only for `INT` and `FLOAT`
///   (`OP_BIT_NEGATE` for `INT` only) — **not** for `BOOL`. The earlier E1 cut incorrectly widened
///   Bool to Int here, which would have folded `-true` to `Int(-1)` even though Godot rejects it.
/// * `OP_NOT` is registered across every type via `Variant::booleanize` and so accepts any operand.
fn eval_unary(op: UnaryOp, v: &FoldedValue) -> Option<FoldedValue> {
    use FoldedValue::*;
    Some(match (op, v) {
        // OP_NEGATE / OP_POSITIVE / OP_BIT_NEGATE — Int/Float only, no Bool widening.
        (UnaryOp::Negative, Int(i)) => Int(i.wrapping_neg()),
        (UnaryOp::Negative, Float(f)) => Float(-f),
        (UnaryOp::Positive, Int(i)) => Int(*i),
        (UnaryOp::Positive, Float(f)) => Float(*f),
        (UnaryOp::Complement, Int(i)) => Int(!i),
        // OP_NOT: defined on every type; returns BOOL. Per `Variant::booleanize`.
        (UnaryOp::LogicNot, Nil) => Bool(true),
        (UnaryOp::LogicNot, Bool(b)) => Bool(!b),
        (UnaryOp::LogicNot, Int(i)) => Bool(*i == 0),
        (UnaryOp::LogicNot, Float(f)) => Bool(*f == 0.0),
        (UnaryOp::LogicNot, String(s)) => Bool(s.is_empty()),
        // Every other (op, kind) pair has no defined Variant evaluation.
        _ => return None,
    })
}

// ===================================================================================================
// reduce_binary_op — analyzer.cpp:3089
// ===================================================================================================

/// `GDScriptAnalyzer::reduce_binary_op` (analyzer.cpp:3089). E1 reproduces:
/// * the recurse-then-bail-on-unset-types guard (analyzer.cpp:3102-3104),
/// * the both-constant fold path (analyzer.cpp:3118-3143) over our `FoldedValue` subset,
/// * the `== nil` / `!= nil` ⇒ BOOL special-case (analyzer.cpp:3147-3152),
/// * the `STRING % …` ⇒ STRING special-case (analyzer.cpp:3153-3157),
/// * the variant-operand-degrades-to-`Variant` arm (analyzer.cpp:3158-3161).
///
/// The hard-typed both-set arm at analyzer.cpp:3162-3169 (the call to `get_operation_type` that
/// produces the `Invalid operands "%s" and "%s" for "%s" operator.` error) lands in E2 with the
/// operator type matrix; for now its outputs degrade to `Variant`, matching the project's "no
/// phantom errors before the validity matrix is in" rule.
fn reduce_binary_op(ctx: &mut AnalysisContext, id: NodeId) {
    let NodeKind::BinaryOp(op_node) = ctx.node(id).kind.clone() else {
        return;
    };

    if let Some(l) = op_node.left_operand {
        reduce_expression(ctx, l, false);
    }
    if let Some(r) = op_node.right_operand {
        reduce_expression(ctx, r, false);
    }

    let left_dt = op_node
        .left_operand
        .map(|n| ctx.get_type(n).clone())
        .unwrap_or_default();
    let right_dt = op_node
        .right_operand
        .map(|n| ctx.get_type(n).clone())
        .unwrap_or_default();

    if !left_dt.is_set() || !right_dt.is_set() {
        // Match Godot: leave the result Unresolved; the dispatcher tail-guard paints it Variant.
        return;
    }

    // INTEGER_DIVISION (analyzer.cpp:3104-3113): `/` over int (or integer-vector) operands
    // discards the decimal part. Reads `builtin_type` directly with no kind gate, as upstream —
    // non-builtin kinds carry `Object` there and never match.
    if op_node.operation == BinaryOp::Division
        && matches!(
            left_dt.builtin_type,
            VariantType::Int
                | VariantType::Vector2i
                | VariantType::Vector3i
                | VariantType::Vector4i
        )
        && (right_dt.builtin_type == VariantType::Int
            || right_dt.builtin_type == left_dt.builtin_type)
    {
        ctx.push_warning(crate::warnings::WarningCode::IntegerDivision, &[], id);
    }

    // Both-constant fold path (analyzer.cpp:3118-3143). Our `FoldedValue` set has no sharing
    // semantics — every value is by-value — so the `!is_shared()` guards are trivially satisfied.
    // (Array/Dictionary literals do NOT get a `FoldedValue`, so `is_reduced` is false for them —
    // matching Godot's `is_shared()` skip; those go through the type-only path below.)
    //
    // An `Opaque` fold (builtin named constant — value unknown, kind known) can't be evaluated;
    // Godot would fold the real values here. Instead the pair drops to the type-only tail below,
    // which validates against the same operator table `Variant::evaluate` dispatches over — the
    // operand kinds are remembered so the invalid arm keeps Godot's constant-operand error
    // template, and a valid result re-stamps `Opaque` so the expression stays constant for
    // `const`/match contexts.
    let mut opaque_operand_types: Option<(VariantType, VariantType)> = None;
    if let (Some(l), Some(r)) = (op_node.left_operand, op_node.right_operand) {
        if ctx.folds.is_reduced(l) && ctx.folds.is_reduced(r) {
            let lv = ctx.folds.get(l).cloned();
            let rv = ctx.folds.get(r).cloned();
            if let (Some(lv), Some(rv)) = (lv, rv) {
                let has_opaque = matches!(lv, FoldedValue::Opaque(_))
                    || matches!(rv, FoldedValue::Opaque(_))
                    // Constant `"fmt %s" % x` — the format itself isn't evaluated (Godot folds
                    // it; gdls has no formatter), so route through the type tail's str_format
                    // arm and keep constancy via the Opaque result stamp.
                    || (op_node.operation == BinaryOp::Modulo
                        && matches!(lv, FoldedValue::String(_)));
                if has_opaque {
                    opaque_operand_types =
                        Some((folded_variant_type(&lv), folded_variant_type(&rv)));
                } else if let Some(folded) = eval_binary(op_node.operation, &lv, &rv) {
                    let dt = type_from_variant(&folded);
                    ctx.folds.set(id, folded);
                    ctx.set_type(id, dt);
                    return;
                } else {
                    // Fold attempted and failed — Godot's `r_valid = false` arm at
                    // analyzer.cpp:3126-3135. Emit the exact `Invalid operands to operator OP, A and B.`
                    // diagnostic the corpus pins (`errors/invalid_concatenation_bool.gd`,
                    // `errors/bitwise_float_{left,right}_operand.gd`). Variant's evaluator names the
                    // operand types via `Variant::get_type_name`, not `DataType::to_string` (the latter
                    // form is the get_operation_type path at analyzer.cpp:3166).
                    let lname = data_type::variant_type_name(folded_variant_type(&lv));
                    let rname = data_type::variant_type_name(folded_variant_type(&rv));
                    ctx.push_error(
                        format!(
                            "Invalid operands to operator {}, {} and {}.",
                            binary_op_symbol(op_node.operation),
                            lname,
                            rname,
                        ),
                        id,
                    );
                    ctx.set_type(id, variant_dt());
                    return;
                }
            }
        }
    }

    // `== nil` / `!= nil` always returns BOOL (analyzer.cpp:3147-3152).
    let nil_eq = matches!(
        op_node.operation,
        BinaryOp::CompEqual | BinaryOp::CompNotEqual
    ) && (is_nil_builtin(&left_dt) || is_nil_builtin(&right_dt));

    // `STRING % …` always returns STRING (analyzer.cpp:3153-3157).
    let str_format = op_node.operation == BinaryOp::Modulo
        && left_dt.kind == DtKind::Builtin
        && left_dt.builtin_type == VariantType::String;

    let result = if nil_eq {
        DataType {
            type_source: TypeSource::AnnotatedExplicit,
            kind: DtKind::Builtin,
            builtin_type: VariantType::Bool,
            ..Default::default()
        }
    } else if str_format {
        DataType {
            type_source: left_dt.type_source,
            kind: DtKind::Builtin,
            builtin_type: VariantType::String,
            ..Default::default()
        }
    } else if left_dt.is_variant() || right_dt.is_variant() {
        // analyzer.cpp:3158-3161: a variant operand keeps the result variant.
        variant_dt()
    } else {
        // analyzer.cpp:3162-3169 — hard-typed binary op. `get_operation_type` consults the
        // Variant operator-validity table; an unregistered (op, a_type, b_type) triple emits the
        // `Invalid operands "X" and "Y" for "OP" operator.` error.
        let (res_dt, valid) = get_operation_type(op_node.operation, &left_dt, &right_dt);
        if !valid {
            if let Some((lt, rt)) = opaque_operand_types {
                // Constant operands take Godot's `Variant::evaluate r_valid=false` template
                // (analyzer.cpp:3126-3135) even though we validated by type — the value was
                // opaque, but the operands were still constants.
                ctx.push_error(
                    format!(
                        "Invalid operands to operator {}, {} and {}.",
                        binary_op_symbol(op_node.operation),
                        data_type::variant_type_name(lt),
                        data_type::variant_type_name(rt),
                    ),
                    id,
                );
            } else {
                ctx.push_error(
                    format!(
                        r#"Invalid operands "{left_dt}" and "{right_dt}" for "{op}" operator."#,
                        op = binary_op_symbol(op_node.operation),
                    ),
                    id,
                );
            }
        }
        res_dt
    };
    // A valid op over constant-but-opaque operands is itself a constant of known kind (Godot
    // folds the real value; the invalid arm returns a Variant-kinded result, so the Builtin
    // gate below excludes it).
    if opaque_operand_types.is_some() && result.kind == DtKind::Builtin {
        ctx.folds.set(id, FoldedValue::Opaque(result.builtin_type));
    }
    ctx.set_type(id, result);
}

/// `GDScriptAnalyzer::get_operation_type(op, a, b, &valid, src)` (analyzer.cpp:6223). Mirrors
/// Godot's delegate to `Variant::get_validated_operator_evaluator` + `get_operator_return_type`
/// — its table is registered in `core/variant/variant_op.cpp` by `register_op<...>` calls; we
/// encode the subset the corpus exercises.
///
/// E3h-scope subset:
///
/// * **LogicAnd / LogicOr** always return `bool` (analyzer.cpp:6224-6232 — they short-circuit and
///   don't go through the table).
/// * **Enum coercion** — analyzer.cpp:6238-6251: an Enum meta acts as Dictionary, an Enum value
///   acts as Int. Done in [`coerce_enum_to_builtin_for_op`].
/// * **`OP_ADD ARRAY × ARRAY`** with matching `container_element_type[0]` — the
///   special case at analyzer.cpp:6256-6263, preserving the element type.
/// * **Validated-operator table** — [`validated_op_result`] returns `Some(result_type)` for the
///   pairs `core/variant/variant_op.cpp` registers, `None` otherwise. `None` means **invalid**
///   for hard-typed operands; the caller emits the error.
fn get_operation_type(op: BinaryOp, a: &DataType, b: &DataType) -> (DataType, bool) {
    // analyzer.cpp:6224-6232 — logic ops always return bool, regardless of operand types.
    if matches!(op, BinaryOp::LogicAnd | BinaryOp::LogicOr) {
        return (
            DataType {
                type_source: TypeSource::AnnotatedInferred,
                kind: DtKind::Builtin,
                builtin_type: VariantType::Bool,
                ..Default::default()
            },
            true,
        );
    }

    // analyzer.cpp:6238-6251 — enum-meta acts as Dictionary, enum-value acts as Int.
    let a_t = coerce_enum_to_builtin_for_op(a);
    let b_t = coerce_enum_to_builtin_for_op(b);
    let hard_operation = a.is_hard_type() && b.is_hard_type();

    // analyzer.cpp:6256-6263 — `OP_ADD ARRAY × ARRAY` with matching `container_element_type[0]`
    // returns the typed-array left operand. gdls's container_element_types is exposed on
    // DataType; the typed-collection slice fills it on Array[T] / Dictionary[K, V] literals.
    if op == BinaryOp::Addition && a_t == VariantType::Array && b_t == VariantType::Array {
        let a_has = !a.container_element_types.is_empty();
        let b_has = !b.container_element_types.is_empty();
        if a_has && b_has && a.container_element_types[0] == b.container_element_types[0] {
            let mut result = a.clone();
            result.type_source = if hard_operation {
                TypeSource::AnnotatedInferred
            } else {
                TypeSource::Inferred
            };
            return (result, true);
        }
    }

    match validated_op_result(op, a_t, b_t) {
        Some(res_bt) => {
            let result = DataType {
                type_source: if hard_operation {
                    TypeSource::AnnotatedInferred
                } else {
                    TypeSource::Inferred
                },
                kind: DtKind::Builtin,
                builtin_type: res_bt,
                ..Default::default()
            };
            (result, true)
        }
        None => {
            // analyzer.cpp:6273-6276 — invalid is gated on `hard_operation`. Soft operands return
            // Variant with `valid=true` (the result is unsafe but not an error).
            (DataType::variant(), !hard_operation)
        }
    }
}

/// Map an `AssignOp` to the equivalent `BinaryOp` for compatibility checks. Mirrors Godot's
/// `AssignmentNode::variant_op` field which the parser populates from the assignment-operator
/// token. `AssignOp::None` (`=`) has no binary counterpart, so the caller checks for that case
/// before invoking this helper.
fn binary_op_for_assign_op(op: gd_syntax::ast::AssignOp) -> Option<BinaryOp> {
    use gd_syntax::ast::AssignOp;
    Some(match op {
        AssignOp::None => return None,
        AssignOp::Addition => BinaryOp::Addition,
        AssignOp::Subtraction => BinaryOp::Subtraction,
        AssignOp::Multiplication => BinaryOp::Multiplication,
        AssignOp::Division => BinaryOp::Division,
        AssignOp::Modulo => BinaryOp::Modulo,
        AssignOp::Power => BinaryOp::Power,
        AssignOp::BitShiftLeft => BinaryOp::BitLeftShift,
        AssignOp::BitShiftRight => BinaryOp::BitRightShift,
        AssignOp::BitAnd => BinaryOp::BitAnd,
        AssignOp::BitOr => BinaryOp::BitOr,
        AssignOp::BitXor => BinaryOp::BitXor,
    })
}

/// analyzer.cpp:6238-6251 — flatten an Enum operand to its underlying builtin (Dictionary for
/// meta, Int for a value) so the operator table can match.
fn coerce_enum_to_builtin_for_op(dt: &DataType) -> VariantType {
    if dt.kind == DtKind::Enum {
        if dt.is_meta_type {
            VariantType::Dictionary
        } else {
            VariantType::Int
        }
    } else {
        dt.builtin_type
    }
}

/// Return the builtin result type for a registered `Variant::evaluate` (op, a, b) triple, or
/// `None` if no entry exists in `core/variant/variant_op.cpp`. We cover the registrations the
/// corpus exercises — the arithmetic / bitwise / string / vector / comparison / in / is family.
/// Other registrations (Transform2D matrix multiply, AABB intersect, ...) stay outside this slice;
/// for an unknown pair we return `None` and the caller treats it as invalid (matches Godot's
/// "no registered evaluator" path at variant_op.cpp register-time).
fn validated_op_result(op: BinaryOp, a: VariantType, b: VariantType) -> Option<VariantType> {
    use BinaryOp::*;
    use VariantType::*;
    match op {
        // --- Arithmetic ---------------------------------------------------------------------
        // OP_ADD / OP_SUBTRACT / OP_MULTIPLY / OP_DIVIDE / OP_MODULE / OP_POWER. See
        // variant_op.cpp:218-365 + 366-414 (modulus/string-format) + 415-440 (power).
        Addition => arith_add(a, b),
        Subtraction => arith_sub(a, b),
        Multiplication => arith_mul(a, b),
        Division => arith_div(a, b),
        Modulo => arith_mod(a, b),
        Power => arith_pow(a, b),

        // --- Bitwise (variant_op.cpp:442-466) ----------------------------------------------
        BitLeftShift | BitRightShift => {
            if a == Int && b == Int {
                Some(Int)
            } else {
                None
            }
        }
        BitAnd | BitOr | BitXor => {
            // Int × Int → Int; Bool × Bool → Bool (& | ^ register for both — variant_op.cpp:456-465).
            if a == Int && b == Int {
                Some(Int)
            } else if a == Bool && b == Bool {
                Some(Bool)
            } else {
                None
            }
        }

        // --- Comparisons (variant_op.cpp:467-744). Godot's table is broad — nearly every
        // typed pair registers `==` / `!=` (registered for many cross-type combos that yield
        // false) plus the same-type ordering ops. We accept any pair as valid (returning Bool)
        // — the corpus doesn't pin invalid-comparison errors and being lenient here matches
        // Godot's permissive `==`/`!=` registry.
        CompEqual | CompNotEqual | CompLess | CompLessEqual | CompGreater | CompGreaterEqual => {
            Some(Bool)
        }

        // --- Logic — caught earlier. Branch unreachable in practice.
        LogicAnd | LogicOr => Some(Bool),

        // --- `in` — variant_op.cpp:880-960. Always Bool (string-find / array-contains /
        // dictionary-has all return bool).
        ContentTest => Some(Bool),
    }
}

// Per-operator validity tables. Each returns `Some(result_type)` for a registered pair, `None`
// otherwise.

fn arith_add(a: VariantType, b: VariantType) -> Option<VariantType> {
    use VariantType::*;
    Some(match (a, b) {
        (Int, Int) => Int,
        (Int, Float) | (Float, Int) | (Float, Float) => Float,
        // register_string_op(OP_ADD) at variant_op.cpp:222 — STRING × {STRING,STRING_NAME} and
        // STRING_NAME × {STRING,STRING_NAME} all return the LEFT operand's type.
        (String, String) | (String, StringName) => String,
        (StringName, String) | (StringName, StringName) => StringName,
        (NodePath, NodePath) => NodePath,
        (Vector2, Vector2) => Vector2,
        (Vector2i, Vector2i) => Vector2i,
        (Vector3, Vector3) => Vector3,
        (Vector3i, Vector3i) => Vector3i,
        (Vector4, Vector4) => Vector4,
        (Vector4i, Vector4i) => Vector4i,
        (Quaternion, Quaternion) => Quaternion,
        (Color, Color) => Color,
        // Array × Array — analyzer.cpp:6256-6263 carries the typed-element narrowing on top of
        // this; the bare `Array` path (no element type or mismatched element) yields Array too.
        (Array, Array) => Array,
        // Packed*Array × same — variant_op.cpp:232-241.
        (PackedByteArray, PackedByteArray) => PackedByteArray,
        (PackedInt32Array, PackedInt32Array) => PackedInt32Array,
        (PackedInt64Array, PackedInt64Array) => PackedInt64Array,
        (PackedFloat32Array, PackedFloat32Array) => PackedFloat32Array,
        (PackedFloat64Array, PackedFloat64Array) => PackedFloat64Array,
        (PackedStringArray, PackedStringArray) => PackedStringArray,
        (PackedVector2Array, PackedVector2Array) => PackedVector2Array,
        (PackedVector3Array, PackedVector3Array) => PackedVector3Array,
        (PackedColorArray, PackedColorArray) => PackedColorArray,
        (PackedVector4Array, PackedVector4Array) => PackedVector4Array,
        _ => return None,
    })
}

fn arith_sub(a: VariantType, b: VariantType) -> Option<VariantType> {
    use VariantType::*;
    Some(match (a, b) {
        (Int, Int) => Int,
        (Int, Float) | (Float, Int) | (Float, Float) => Float,
        (Vector2, Vector2) => Vector2,
        (Vector2i, Vector2i) => Vector2i,
        (Vector3, Vector3) => Vector3,
        (Vector3i, Vector3i) => Vector3i,
        (Vector4, Vector4) => Vector4,
        (Vector4i, Vector4i) => Vector4i,
        (Quaternion, Quaternion) => Quaternion,
        (Color, Color) => Color,
        _ => return None,
    })
}

fn arith_mul(a: VariantType, b: VariantType) -> Option<VariantType> {
    use VariantType::*;
    Some(match (a, b) {
        (Int, Int) => Int,
        (Int, Float) | (Float, Int) | (Float, Float) => Float,
        // Vector × scalar / scalar × Vector (variant_op.cpp:258-340) — keep the vector type, but
        // an INT × VectorI uses INT-typed component math while FLOAT × VectorI returns Vector (not
        // VectorI). The corpus doesn't pin these subtleties; we return the vector type either way.
        (Vector2, Vector2)
        | (Int, Vector2)
        | (Vector2, Int)
        | (Float, Vector2)
        | (Vector2, Float) => Vector2,
        (Vector2i, Vector2i) | (Int, Vector2i) | (Vector2i, Int) => Vector2i,
        (Float, Vector2i) | (Vector2i, Float) => Vector2,
        (Vector3, Vector3)
        | (Int, Vector3)
        | (Vector3, Int)
        | (Float, Vector3)
        | (Vector3, Float) => Vector3,
        (Vector3i, Vector3i) | (Int, Vector3i) | (Vector3i, Int) => Vector3i,
        (Float, Vector3i) | (Vector3i, Float) => Vector3,
        (Vector4, Vector4)
        | (Int, Vector4)
        | (Vector4, Int)
        | (Float, Vector4)
        | (Vector4, Float) => Vector4,
        (Vector4i, Vector4i) | (Int, Vector4i) | (Vector4i, Int) => Vector4i,
        (Float, Vector4i) | (Vector4i, Float) => Vector4,
        (Color, Color) | (Color, Int) | (Color, Float) | (Int, Color) | (Float, Color) => Color,
        (Quaternion, Quaternion)
        | (Quaternion, Int)
        | (Quaternion, Float)
        | (Int, Quaternion)
        | (Float, Quaternion) => Quaternion,
        // Quaternion rotates a Vector3 (variant_op.cpp:301-303, XForm/XFormInv pair).
        (Quaternion, Vector3) | (Vector3, Quaternion) => Vector3,
        // Matrix multiplies + xform/xform_inv pairs (variant_op.cpp:306-335). The xform result
        // is the OPERAND type (Transform2D * Vector2 → Vector2); matrix × matrix / scalar keeps
        // the matrix.
        (Transform2d, Transform2d) | (Transform2d, Int) | (Transform2d, Float) => Transform2d,
        (Transform2d, Vector2) | (Vector2, Transform2d) => Vector2,
        (Transform2d, Rect2) | (Rect2, Transform2d) => Rect2,
        (Transform2d, PackedVector2Array) | (PackedVector2Array, Transform2d) => PackedVector2Array,
        (Transform3d, Transform3d) | (Transform3d, Int) | (Transform3d, Float) => Transform3d,
        (Transform3d, Vector3) | (Vector3, Transform3d) => Vector3,
        (Transform3d, Aabb) | (Aabb, Transform3d) => Aabb,
        (Transform3d, Plane) | (Plane, Transform3d) => Plane,
        (Transform3d, PackedVector3Array) | (PackedVector3Array, Transform3d) => PackedVector3Array,
        (Projection, Projection) => Projection,
        (Projection, Vector4) | (Vector4, Projection) => Vector4,
        (Basis, Basis) | (Basis, Int) | (Basis, Float) => Basis,
        (Basis, Vector3) | (Vector3, Basis) => Vector3,
        _ => return None,
    })
}

fn arith_div(a: VariantType, b: VariantType) -> Option<VariantType> {
    use VariantType::*;
    Some(match (a, b) {
        (Int, Int) => Int,
        (Int, Float) | (Float, Int) | (Float, Float) => Float,
        (Vector2, Vector2) | (Vector2, Int) | (Vector2, Float) => Vector2,
        (Vector2i, Vector2i) | (Vector2i, Int) => Vector2i,
        (Vector2i, Float) => Vector2,
        (Vector3, Vector3) | (Vector3, Int) | (Vector3, Float) => Vector3,
        (Vector3i, Vector3i) | (Vector3i, Int) => Vector3i,
        (Vector3i, Float) => Vector3,
        (Vector4, Vector4) | (Vector4, Int) | (Vector4, Float) => Vector4,
        (Vector4i, Vector4i) | (Vector4i, Int) => Vector4i,
        (Vector4i, Float) => Vector4,
        (Color, Color) | (Color, Int) | (Color, Float) => Color,
        (Quaternion, Float) | (Quaternion, Int) => Quaternion,
        _ => return None,
    })
}

fn arith_mod(a: VariantType, b: VariantType) -> Option<VariantType> {
    use VariantType::*;
    // variant_op.cpp:366-414. Modulo registers for INT × INT, FLOAT × FLOAT (math.fmod), Vector*i
    // pairs, and the string-format variants (STRING/STRING_NAME % everything → STRING).
    Some(match (a, b) {
        (Int, Int) => Int,
        (Float, Float) => Float,
        (Vector2i, Vector2i) | (Vector2i, Int) => Vector2i,
        (Vector3i, Vector3i) | (Vector3i, Int) => Vector3i,
        (Vector4i, Vector4i) | (Vector4i, Int) => Vector4i,
        // String formatting — Variant::OP_MODULE STRING/STRING_NAME × everything = STRING. The
        // caller's `str_format` short-circuit at reduce_binary_op already produces String for
        // String %; we still register it here for completeness when the operands flip.
        (String, _) | (StringName, _) => String,
        _ => return None,
    })
}

fn arith_pow(a: VariantType, b: VariantType) -> Option<VariantType> {
    use VariantType::*;
    Some(match (a, b) {
        (Int, Int) => Int,
        (Int, Float) | (Float, Int) | (Float, Float) => Float,
        _ => return None,
    })
}

/// Whether a [`DataType`] is the builtin `Nil` type (Godot's `null`/`void` constant).
fn is_nil_builtin(dt: &DataType) -> bool {
    dt.kind == DtKind::Builtin && dt.builtin_type == VariantType::Nil
}

/// Constant-fold a binary operation over our `FoldedValue` subset, mirroring `Variant::evaluate`
/// for the `(op, left-type, right-type)` triples that are registered in `core/variant/variant_op.cpp`.
/// Returns `None` when the triple has no registered evaluator — Godot's `r_valid = false` path.
///
/// **Bool is intentionally not widened to Int** for arithmetic/bitwise: Godot registers
/// `OP_ADD`/`OP_SUBTRACT`/`OP_MULTIPLY`/`OP_DIVIDE`/`OP_MODULE`/`OP_POWER` and the bitwise family
/// only for `INT × INT`, `INT × FLOAT`, `FLOAT × INT`, `FLOAT × FLOAT`, plus `OP_ADD STRING × STRING`
/// (variant_op.cpp:218-256). `BOOL × BOOL` for arithmetic is therefore an error — exactly what
/// `errors/invalid_concatenation_bool.gd` (`print(true + true)`) pins.
fn eval_binary(op: BinaryOp, a: &FoldedValue, b: &FoldedValue) -> Option<FoldedValue> {
    use FoldedValue::*;
    // An `Opaque` operand has no materialized value to evaluate — `reduce_binary_op` routes those
    // to the type-only path before calling here; this guard keeps the function total (and keeps
    // the booleanize-based logic arms below from fabricating a value).
    if matches!(a, Opaque(_)) || matches!(b, Opaque(_)) {
        return None;
    }
    // Comparisons (`==`, `!=`, `<`, `<=`, `>`, `>=`) — Variant registers comparisons for many type
    // pairs incl. Bool×Bool (variant_op.cpp:488/610/731/762). The mixed-numeric (Int↔Bool/Float)
    // cases also have entries; `compare` mirrors them by widening through `to_float` for any pair
    // where both operands are numeric-or-bool.
    if matches!(
        op,
        BinaryOp::CompEqual
            | BinaryOp::CompNotEqual
            | BinaryOp::CompLess
            | BinaryOp::CompLessEqual
            | BinaryOp::CompGreater
            | BinaryOp::CompGreaterEqual
    ) {
        return compare(op, a, b);
    }

    // Logic short-circuits — `Variant::evaluate` defines OP_AND/OP_OR over `booleanize()` for every
    // pair, so these are always foldable when both operands fold.
    match (op, a, b) {
        (BinaryOp::LogicAnd, _, _) => return Some(Bool(booleanize(a) && booleanize(b))),
        (BinaryOp::LogicOr, _, _) => return Some(Bool(booleanize(a) || booleanize(b))),
        _ => (),
    }

    // String concatenation (`+` on String) — `register_string_op(OP_ADD)` at variant_op.cpp:222.
    if op == BinaryOp::Addition {
        if let (String(l), String(r)) = (a, b) {
            return Some(String(format!("{l}{r}")));
        }
    }

    // Arithmetic — Int×Int, Int×Float, Float×Int, Float×Float only. Bool/String/Nil mixed with
    // anything (or themselves, for arithmetic) is NOT registered ⇒ None.
    match (a, b) {
        (Int(l), Int(r)) => match op {
            BinaryOp::Addition => Some(Int(l.wrapping_add(*r))),
            BinaryOp::Subtraction => Some(Int(l.wrapping_sub(*r))),
            BinaryOp::Multiplication => Some(Int(l.wrapping_mul(*r))),
            BinaryOp::Division if *r != 0 => Some(Int(l.wrapping_div(*r))),
            BinaryOp::Modulo if *r != 0 => Some(Int(l.wrapping_rem(*r))),
            BinaryOp::Power if *r >= 0 => Some(Int((l).pow(*r as u32))),
            BinaryOp::BitLeftShift if (0..64).contains(r) => Some(Int(l.wrapping_shl(*r as u32))),
            BinaryOp::BitRightShift if (0..64).contains(r) => Some(Int(l.wrapping_shr(*r as u32))),
            BinaryOp::BitAnd => Some(Int(l & r)),
            BinaryOp::BitOr => Some(Int(l | r)),
            BinaryOp::BitXor => Some(Int(l ^ r)),
            _ => None,
        },
        // At least one Float, both numeric (no Bool/String).
        (Int(_), Float(_)) | (Float(_), Int(_)) | (Float(_), Float(_)) => {
            let lf = match a {
                Int(v) => *v as f64,
                Float(v) => *v,
                _ => return None,
            };
            let rf = match b {
                Int(v) => *v as f64,
                Float(v) => *v,
                _ => return None,
            };
            match op {
                BinaryOp::Addition => Some(Float(lf + rf)),
                BinaryOp::Subtraction => Some(Float(lf - rf)),
                BinaryOp::Multiplication => Some(Float(lf * rf)),
                BinaryOp::Division if rf != 0.0 => Some(Float(lf / rf)),
                BinaryOp::Modulo if rf != 0.0 => Some(Float(lf.rem_euclid(rf))),
                BinaryOp::Power => Some(Float(lf.powf(rf))),
                // Bitwise on Float — exactly the `errors/bitwise_float_*` case ⇒ None.
                _ => None,
            }
        }
        // Bool×Bool arithmetic/bitwise / String mixed / Nil mixed — unregistered.
        _ => None,
    }
}

/// Variant comparison over our `FoldedValue` subset — numeric is loose (Int↔Float mixed), every
/// other pair compares by exact-type structural equality.
fn compare(op: BinaryOp, a: &FoldedValue, b: &FoldedValue) -> Option<FoldedValue> {
    use std::cmp::Ordering;
    use FoldedValue::*;

    // Numeric mixed-mode (Int+Bool, Int+Float, …) — compare as f64. Everything else needs an
    // exact-kind match; mixing kinds yields false-equal / true-not-equal (Variant's behavior).
    let cmp_num = if let (Some(l), Some(r)) = (to_float(a), to_float(b)) {
        if matches!(a, Int(_) | Float(_) | Bool(_)) && matches!(b, Int(_) | Float(_) | Bool(_)) {
            Some(if l < r {
                Ordering::Less
            } else if l > r {
                Ordering::Greater
            } else {
                Ordering::Equal
            })
        } else {
            None
        }
    } else {
        None
    };

    let cmp = cmp_num.or_else(|| match (a, b) {
        (Nil, Nil) => Some(Ordering::Equal),
        (String(l), String(r)) => Some(l.cmp(r)),
        _ => None,
    });

    Some(match (op, cmp) {
        (BinaryOp::CompEqual, Some(Ordering::Equal)) => Bool(true),
        (BinaryOp::CompEqual, Some(_)) => Bool(false),
        (BinaryOp::CompEqual, None) => Bool(false), // mismatched kinds compare unequal
        (BinaryOp::CompNotEqual, Some(Ordering::Equal)) => Bool(false),
        (BinaryOp::CompNotEqual, Some(_)) => Bool(true),
        (BinaryOp::CompNotEqual, None) => Bool(true),
        (BinaryOp::CompLess, Some(Ordering::Less)) => Bool(true),
        (BinaryOp::CompLess, Some(_)) => Bool(false),
        (BinaryOp::CompLessEqual, Some(Ordering::Greater)) => Bool(false),
        (BinaryOp::CompLessEqual, Some(_)) => Bool(true),
        (BinaryOp::CompGreater, Some(Ordering::Greater)) => Bool(true),
        (BinaryOp::CompGreater, Some(_)) => Bool(false),
        (BinaryOp::CompGreaterEqual, Some(Ordering::Less)) => Bool(false),
        (BinaryOp::CompGreaterEqual, Some(_)) => Bool(true),
        // Ordered comparisons across incomparable kinds: `Variant::evaluate` would set
        // `r_valid=false`. We mirror that with `None` so the binary-op caller leaves the type
        // unfolded and degrades to `Variant`.
        _ => return None,
    })
}

/// `Variant::booleanize` over our subset — every value falsy/truthy as the engine sees it.
/// `pub(crate)` for `resolve_assert`'s ASSERT_ALWAYS_TRUE/_FALSE constant-condition check.
pub(crate) fn booleanize(v: &FoldedValue) -> bool {
    use FoldedValue::*;
    match v {
        Nil => false,
        Bool(b) => *b,
        Int(i) => *i != 0,
        Float(f) => *f != 0.0,
        String(s) => !s.is_empty(),
        // Unreachable in practice: `eval_binary` rejects Opaque operands before its
        // booleanize-driven logic arms run. Total for safety; never trust an unknown value.
        Opaque(_) => false,
    }
}

/// The [`VariantType`] of a folded value — the same mapping [`type_from_variant`] uses internally.
/// Lives separately so the binary-op error path can name operand types without rebuilding the
/// full `DataType`.
fn folded_variant_type(v: &FoldedValue) -> VariantType {
    match v {
        FoldedValue::Nil => VariantType::Nil,
        FoldedValue::Bool(_) => VariantType::Bool,
        FoldedValue::Int(_) => VariantType::Int,
        FoldedValue::Float(_) => VariantType::Float,
        FoldedValue::String(_) => VariantType::String,
        FoldedValue::Opaque(vt) => *vt,
    }
}

/// Godot's `_op_names` table (variant_op.cpp:1081) for the binary operator symbols — `+`/`-`/`<<`
/// etc. — that appear in the error messages at analyzer.cpp:3128-3134. Unary ops have their own
/// entries (`unary-`/`unary+`/`~`/`not`) that we don't need until [`reduce_unary_op`] emits errors.
fn binary_op_symbol(op: BinaryOp) -> &'static str {
    match op {
        BinaryOp::CompEqual => "==",
        BinaryOp::CompNotEqual => "!=",
        BinaryOp::CompLess => "<",
        BinaryOp::CompLessEqual => "<=",
        BinaryOp::CompGreater => ">",
        BinaryOp::CompGreaterEqual => ">=",
        BinaryOp::Addition => "+",
        BinaryOp::Subtraction => "-",
        BinaryOp::Multiplication => "*",
        BinaryOp::Division => "/",
        BinaryOp::Modulo => "%",
        BinaryOp::Power => "**",
        BinaryOp::BitLeftShift => "<<",
        BinaryOp::BitRightShift => ">>",
        BinaryOp::BitAnd => "&",
        BinaryOp::BitOr => "|",
        BinaryOp::BitXor => "^",
        BinaryOp::LogicAnd => "and",
        BinaryOp::LogicOr => "or",
        BinaryOp::ContentTest => "in",
    }
}

/// Lift a comparable [`FoldedValue`] to `f64` for the [`compare`] mixed-numeric path. `Bool` widens
/// to `f64` because comparison (unlike arithmetic) IS registered for `BOOL × {BOOL,INT,FLOAT}` in
/// the Variant op table (variant_op.cpp:731/762 + cross-type entries).
fn to_float(v: &FoldedValue) -> Option<f64> {
    match v {
        FoldedValue::Int(i) => Some(*i as f64),
        FoldedValue::Float(f) => Some(*f),
        FoldedValue::Bool(b) => Some(*b as i64 as f64),
        _ => None,
    }
}

// ===================================================================================================
// reduce_array — analyzer.cpp:2695
// ===================================================================================================

/// `GDScriptAnalyzer::reduce_array` (analyzer.cpp:2695): recurse on every element, then assign the
/// node the constant `Array` builtin type. The typed-array element-coercion path
/// (`update_array_literal_element_type`, analyzer.cpp:2775) needs `is_type_compatible` and lands in
/// E3 with the assignment reducer; until then an array literal is unparameterized `Array`,
/// matching Godot's first-pass type before contextual narrowing.
fn reduce_array(ctx: &mut AnalysisContext, id: NodeId) {
    let elements: Vec<NodeId> = match &ctx.node(id).kind {
        NodeKind::Array(a) => a.elements.clone(),
        _ => return,
    };
    for el in elements {
        reduce_expression(ctx, el, false);
    }
    ctx.set_type(
        id,
        DataType {
            type_source: TypeSource::AnnotatedExplicit,
            kind: DtKind::Builtin,
            builtin_type: VariantType::Array,
            is_constant: true,
            ..Default::default()
        },
    );
}

// ===================================================================================================
// reduce_dictionary — analyzer.cpp:3819
// ===================================================================================================

/// `GDScriptAnalyzer::reduce_dictionary` (analyzer.cpp:3819): recurse on every key/value, detect
/// duplicate constant keys, then assign the node the constant `Dictionary` builtin type. The
/// element-coercion pass (`update_dictionary_literal_element_type`, analyzer.cpp:2806) lands in a
/// later E3 slice with the rest of the assignment family.
///
/// Lua- vs Python-style key handling matches Godot (analyzer.cpp:3825-3829 +
/// gdscript_parser.cpp:3313-3319):
/// * Python style (`"k": v`): the key is a real expression — run [`reduce_expression`].
/// * Lua style (`k = v` or `"k" = v`): the **parser** pre-folds the key in Godot
///   (`gdscript_parser.cpp:3313` sets `key->is_constant = true` + writes `reduced_value`). gdls's
///   parser is engine-free, so we mirror the same pre-folding here — an `Identifier` key folds to
///   its name, a `String`/`StringName`/`NodePath` literal folds to its value, anything else stays
///   un-folded.
///
/// `StringName ↔ String` equivalence is automatic: [`folded_from_literal`] coalesces both into
/// `FoldedValue::String`, so the corpus case `errors/dictionary_string_stringname_equivalent.gd`
/// (`&"key"` ≡ `"key"`) is caught.
fn reduce_dictionary(ctx: &mut AnalysisContext, id: NodeId) {
    let dict = match &ctx.node(id).kind {
        NodeKind::Dictionary(d) => d.clone(),
        _ => return,
    };

    // (folded key, the parser-line of the first occurrence) — linear-scan because the cardinality
    // of a single literal dictionary is small, and `FoldedValue` isn't `Eq + Hash` (Float).
    let mut seen: Vec<(FoldedValue, u32)> = Vec::with_capacity(dict.elements.len());

    for kv in &dict.elements {
        if let Some(k) = kv.key {
            match dict.style {
                Some(gd_syntax::ast::DictStyle::PythonDict) => {
                    reduce_expression(ctx, k, false);
                }
                Some(gd_syntax::ast::DictStyle::LuaTable) | None => {
                    fold_lua_dict_key(ctx, k);
                }
            }
        }
        if let Some(v) = kv.value {
            reduce_expression(ctx, v, false);
        }

        // Dup-key check (analyzer.cpp:3829-3837) — only on keys that folded to a constant value.
        if let Some(k) = kv.key {
            if let Some(folded_key) = ctx.folds.get(k).cloned() {
                if let Some((_, prev_line)) =
                    seen.iter().find(|(v, _)| folded_value_eq(v, &folded_key))
                {
                    let label = folded_key_display(&folded_key);
                    ctx.push_error(
                        format!(
                            r#"Key "{label}" was already used in this dictionary (at line {prev_line})."#
                        ),
                        k,
                    );
                } else {
                    let line = ctx.node(k).loc.start.line;
                    seen.push((folded_key, line));
                }
            }
        }
    }

    ctx.set_type(
        id,
        DataType {
            type_source: TypeSource::AnnotatedExplicit,
            kind: DtKind::Builtin,
            builtin_type: VariantType::Dictionary,
            is_constant: true,
            ..Default::default()
        },
    );
}

/// Pre-fold a lua-style dictionary key — Godot's `gdscript_parser.cpp:3313-3319` written at
/// analysis time instead of parse time (gdls's parser is engine-free). Sets the key's folded
/// value, builtin type, and marks the node visited so a later `reduce_expression` no-ops.
fn fold_lua_dict_key(ctx: &mut AnalysisContext, key_id: NodeId) {
    let folded = match &ctx.node(key_id).kind {
        NodeKind::Identifier(i) => FoldedValue::String(i.name.clone()),
        NodeKind::Literal(l) => match &l.value {
            Literal::String(s) | Literal::StringName(s) | Literal::NodePath(s) => {
                FoldedValue::String(s.clone())
            }
            other => folded_from_literal(other),
        },
        _ => return,
    };
    ctx.folds.set(key_id, folded.clone());
    ctx.set_type(key_id, type_from_variant(&folded));
    // Match Godot's "parser pre-marks key reduced": prevent a later reduce_expression from
    // running over (and possibly re-typing) this node.
    ctx.reduced.insert(key_id);
}

/// `FoldedValue` equality for dup-key detection. `PartialEq` on `FoldedValue` already exists, but
/// it would reject `Float` equality on NaN — for dictionary keys that's the right behavior
/// (NaN != NaN), and Godot's Variant `==` on numeric keys mirrors it. Plus
/// `StringName/String/NodePath` are coalesced into `FoldedValue::String` so they compare equal,
/// matching `errors/dictionary_string_stringname_equivalent.gd`.
fn folded_value_eq(a: &FoldedValue, b: &FoldedValue) -> bool {
    // An `Opaque` constant's value is unknown — it can never be *proven* a duplicate, so it never
    // compares equal (`{Vector3.UP: 1, Vector3.DOWN: 2}` must not flag a phantom dup-key; Godot
    // compares the real folded vectors).
    if matches!(a, FoldedValue::Opaque(_)) || matches!(b, FoldedValue::Opaque(_)) {
        return false;
    }
    a == b
}

/// Render a folded key for the `Key "%s"` substitution in the dup-key diagnostic
/// (analyzer.cpp:3831). Godot uses `Variant::stringify`; our small subset uses each value's
/// natural display.
fn folded_key_display(v: &FoldedValue) -> String {
    match v {
        FoldedValue::Nil => "<null>".to_owned(),
        FoldedValue::Bool(b) => b.to_string(),
        FoldedValue::Int(i) => i.to_string(),
        FoldedValue::Float(f) => f.to_string(),
        FoldedValue::String(s) => s.clone(),
        // Unreachable: `folded_value_eq` never matches an Opaque key, so the dup-key error can't
        // name one. Total for safety — render the kind, the only thing we know.
        FoldedValue::Opaque(vt) => data_type::variant_type_name(*vt).to_owned(),
    }
}

// ===================================================================================================
// reduce_identifier — analyzer.cpp:4363
// ===================================================================================================

/// `GDScriptAnalyzer::reduce_identifier(p_identifier, can_be_builtin)` (analyzer.cpp:4363).
///
/// E3c lands the local + parameter + class-member + base-class-member arms. The reducer **does
/// not** yet emit Godot's `Identifier "%s" not declared in the current scope.` error at
/// analyzer.cpp:4658-4660 because the global lookups it gates on (utility functions, global
/// constants, global enums, autoload singletons) aren't ported yet — emitting now would
/// false-positive on every `print(...)` / `Color.RED` / `OS.get_*()` call across the corpus.
/// Unresolved identifiers therefore degrade silently to `Variant`, matching the project's "unknown
/// stays dynamic, never a phantom error" rule (`docs/00`).
///
/// Godot's `IdentifierNode::source` back-pointer scheme (FUNCTION_PARAMETER / LOCAL_VARIABLE /
/// MEMBER_*; gdscript_parser.h:1118) is filled by the parser. gdls's AST is engine-free and lacks
/// that field, so this port re-derives the lookup at analysis time by walking
/// [`AnalysisContext::suite_stack`] (locals/iterator binds), then the current function's parameter
/// list, then the current class's members + the in-file class-base chain, then native/global/
/// builtin lookups.
fn reduce_identifier(ctx: &mut AnalysisContext, id: NodeId) {
    let name = match &ctx.node(id).kind {
        NodeKind::Identifier(i) => i.name.clone(),
        _ => return,
    };

    // 1. Suite-local lookup (analyzer.cpp:4416-4434, LOCAL_VARIABLE/LOCAL_CONSTANT/LOCAL_ITERATOR/
    //    LOCAL_BIND). Walk the active suite chain top-to-bottom — innermost wins.
    // Before resolving, check for CONFUSABLE_LOCAL_USAGE (analyzer.cpp:4443-4445): when the
    // current suite has a local with this name but it's declared AFTER the identifier (a
    // forward reference), Godot doesn't resolve it as a local — the identifier instead
    // falls through to the class-member lookup, and the warning fires. gdls's `lookup_local`
    // finds locals regardless of position, so we check the position first and emit the
    // warning if the local is ahead.
    // A "forward local" is an identifier that references a name existing in the current
    // suite, but the local is declared at or after the identifier's position. This covers
    // two cases Godot treats identically: `print(a) ... var a = 2` (the identifier is
    // before the declaration) and `var a = a + 1` (the identifier is inside the same
    // variable's initializer — Godot's parser hasn't added the local yet when parsing
    // the initializer, so it's also UNDEFINED_SOURCE).
    let forward_local = ctx.suite_stack.last().copied().and_then(|sid| {
        if let NodeKind::Suite(s) = &ctx.node(sid).kind {
            if let Some(&idx) = s.locals_indices.get(&name) {
                if let Some(local) = s.locals.get(idx) {
                    let decl_start = ctx.node(local.source).span.start;
                    let ident_pos = ctx.node(id).span.start;
                    if ident_pos < decl_start {
                        return Some(local.clone());
                    }
                    // Inside the same variable's span (initializer reference).
                    let decl_end = ctx.node(local.source).span.end;
                    if ident_pos >= decl_start && ident_pos < decl_end {
                        return Some(local.clone());
                    }
                }
            }
        }
        None
    });
    let is_forward = forward_local.is_some();
    if let Some(ref fwd) = forward_local {
        // Anchor the warning at the identifier node. For the initializer-reference case
        // (`var a = a + 1`), the RHS `a` has a later byte position than the declaration `a`,
        // but Godot's emit order puts CONFUSABLE before SHADOWED. Anchor at the
        // declaration identifier when the usage is inside the declaration's span so gdls's
        // stable byte-position sort preserves Godot's order.
        let decl_span = ctx.node(fwd.source).span;
        let ident_pos = ctx.node(id).span.start;
        let anchor = if ident_pos >= decl_span.start && ident_pos < decl_span.end {
            match &ctx.node(fwd.source).kind {
                NodeKind::Variable(v) => v.identifier.unwrap_or(id),
                NodeKind::Constant(c) => c.identifier.unwrap_or(id),
                _ => id,
            }
        } else {
            id
        };
        ctx.push_warning(
            crate::warnings::WarningCode::ConfusableLocalUsage,
            std::slice::from_ref(&name),
            anchor,
        );
    }

    if !is_forward {
        if let Some(local) = lookup_local(ctx, &name) {
            let dt = ctx.get_type(local.source).clone();
            if local.kind == gd_syntax::ast::LocalKind::Constant {
                if let Some(init) = constant_initializer_of(ctx, local.source) {
                    if let Some(fv) = ctx.folds.get(init).cloned() {
                        ctx.folds.set(id, fv);
                    }
                }
            }
            // UNASSIGNED_VARIABLE (analyzer.cpp:4435-4439, LOCAL_VARIABLE arm): a read of a
            // local variable with zero assignments so far — flow-insensitive: an assignment in
            // a not-yet-resolved later statement doesn't save an earlier read, and once any
            // assignment has been counted, later reads stay silent ("maybe assigned"). Hard
            // builtin types are exempt (they zero-initialize meaningfully).
            if local.kind == gd_syntax::ast::LocalKind::Variable
                && assignment_count(ctx, local.source) == 0
                && !(dt.is_hard_type() && dt.kind == DtKind::Builtin)
            {
                ctx.push_warning(
                    crate::warnings::WarningCode::UnassignedVariable,
                    std::slice::from_ref(&name),
                    id,
                );
            }
            ctx.set_type(id, dt);
            return;
        }
    }

    // 2. Function parameter lookup (analyzer.cpp:4393-4396, FUNCTION_PARAMETER).
    // analyzer.cpp:4471-4475 — a parameter declared *after* the identifier's position (forward
    // reference inside an earlier parameter's default value) doesn't resolve as the parameter;
    // the identifier instead falls through to the not-declared error
    // (`errors/params_default_forward_reference.gd`). Position-based check mirrors Godot's
    // implicit "declared_after" relationship.
    if let Some(fn_id) = ctx.current_function {
        if let Some(param_id) = function_param_named(ctx, fn_id, &name) {
            let ident_pos = ctx.node(id).span.start;
            let param_pos = ctx.node(param_id).span.start;
            if ident_pos >= param_pos {
                let dt = ctx.get_type(param_id).clone();
                ctx.set_type(id, dt);
                return;
            }
        }
    }

    // 3. Class-member + base-chain lookup (analyzer.cpp:4450-4455 → reduce_identifier_from_base).
    if let Some(class_id) = ctx.current_class {
        if let Some((dt, fold)) = lookup_class_member(ctx, class_id, &name, id) {
            // Record on resolution success only — unresolved/forward-reference cases would
            // pollute the bindings. The recorded `target_kind` is reserved for a future kind-aware
            // `references` filter (`Binding::matches_use` / `find_use_bindings`); the v1 handler
            // (`handlers::push_binding_locations`) matches by bare `name` across both Call and Use
            // bindings — intentionally loose, over-report beats under-report — so it does NOT yet
            // consult the kind.
            let site = ctx.node(id).span;
            // WP-RD2: `ctx.file` is already `Option`; an orphan records `None` ("don't know")
            // instead of a colliding placeholder id. WP-RD14: via the `Binding::use_` ctor.
            ctx.record_binding(Binding::use_(
                ctx.file,
                BindingSymbolKind::Member,
                name.clone(),
                site,
            ));
            if let Some(fv) = fold {
                ctx.folds.set(id, fv);
            }
            // No UNASSIGNED_VARIABLE here, deliberately: upstream's warning block sits in the
            // `p_identifier->source` switch (analyzer.cpp:4408-4441), and a member identifier's
            // `source` is classified AFTER that switch on first resolution — so the
            // MEMBER_VARIABLE/STATIC_VARIABLE arms only see re-reduced identifiers and members
            // never warn in practice (the corpus's property/static fixtures pin this). Only
            // parse-time-classified LOCALS fire the warning on first reduce.
            ctx.set_type(id, dt);
            // analyzer.cpp:4464-4490 — when accessing a non-static instance member from a static
            // function (or a static-variable initializer), Godot emits
            // `Cannot access <kind> "<name>" from the static function "<parent>()".` (or
            // `... from a static variable initializer.` when there is no enclosing function).
            // The reduce_call path emits the parallel `Cannot call ...` template at
            // reducer.rs:2322 (WP-I); this is the access counterpart, walked from the same
            // `static_context` flag. Skip when we're inside a Call's pre-reduce of the callee
            // (the call-version of the check will fire there) so we never double-emit.
            if ctx.static_context && !ctx.reducing_callee {
                if let Some(kind) = non_static_instance_member_kind(ctx, class_id, &name) {
                    let parent = enclosing_concrete_function_name(ctx);
                    let kind_label = match kind {
                        NonStaticKind::Variable => "non-static variable",
                        NonStaticKind::Function => "non-static function",
                        NonStaticKind::Signal => "signal",
                    };
                    let msg = if let Some(parent_name) = parent {
                        format!(
                            r#"Cannot access {kind_label} "{name}" from the static function "{parent_name}()"."#
                        )
                    } else {
                        format!(
                            r#"Cannot access {kind_label} "{name}" from a static variable initializer."#
                        )
                    };
                    ctx.push_error(msg, id);
                }
            }
            return;
        }
    }

    // 3.5. Inherited members through a cross-file Script base (the analyzer.cpp:4166-4267
    // script_classes walk continuing past the file boundary). `lookup_class_member` above stops
    // at in-file Class links; when the current class's chain bottoms out in a Script base, the
    // same per-kind member walk continues through `crate::script_chain` — `ping.emit(1)` against
    // a `signal ping` declared in the cross-file base used to be `Identifier "ping" not
    // declared`. Instance context (`p_base == nullptr` ⇒ type_from_metatype lowers to instance,
    // analyzer.cpp:4030-4034), so all member kinds are reachable. Skipped in callee position:
    // `reduce_call` resolves bare inherited calls itself (the in-file class walk + the
    // cross-file CallSig chain), and typing the callee as a constant Callable here would
    // mis-fire Godot's `Name "X" is a Callable` error that is reserved for callable-holding
    // variables.
    if !ctx.reducing_callee {
        if let Some(sr) = current_class_script_base(ctx) {
            if let Some((dt, fold)) = lookup_script_chain_member(ctx, &sr, &name, false, id) {
                if let Some(fv) = fold {
                    ctx.folds.set(id, fv);
                }
                ctx.set_type(id, dt);
                return;
            }
        }
        // Bare native members of the current class's root — `position`, `changed.connect(…)`,
        // an uncalled method reference — Godot reaches the native arms (analyzer.cpp:4308-4360)
        // through the implicit `current_class` base. Skipped in callee position (reduce_call's
        // native-method path owns those).
        if let Some(class_id) = ctx.current_class {
            if let Some(root) = crate::resolver::nearest_native_ancestor(ctx, class_id) {
                if try_native_member(ctx, &root, &name, false, id) {
                    return;
                }
            }
        }
    }

    // 4. Native class name → metatype (analyzer.cpp:4541-4545).
    if ctx.native.class_named(&name).is_some() {
        ctx.set_type(id, native_meta_type(name.clone()));
        return;
    }

    // 5. Project `class_name` → script metatype (analyzer.cpp:4547-4550).
    if let Some(fid) = ctx.xfile.global_class_file(&name) {
        // Cross-file class reference: drives `textDocument/references` and
        // `textDocument/implementation` (the latter then filters by extends chain).
        let site = ctx.node(id).span;
        ctx.record_binding(Binding::use_(
            Some(fid),
            BindingSymbolKind::Class,
            name.clone(),
            site,
        ));
        if Some(fid) == ctx.file {
            let root = ctx
                .tree
                .root_id()
                .map(|r| ctx.get_type(r).clone())
                .unwrap_or_default();
            ctx.set_type(id, root);
        } else {
            ctx.set_type(id, script_meta_type(ctx, fid));
        }
        return;
    }

    // 6. Builtin type name (`int`/`String`/`Vector2`/…). Godot gates on `can_be_builtin`
    //    (analyzer.cpp:4531-4539) and emits "Builtin type cannot be used as a name on its own."
    //    when used standalone. The standalone-error gate needs the call/subscript context the
    //    later E3 slices wire in; for E3c we just expose the metatype so e.g. `int` as a type
    //    annotation operand still resolves.
    if let Some(bt) = crate::resolver::builtin_type_from_name(&name) {
        ctx.set_type(id, builtin_meta_type(bt));
        return;
    }

    // 7. Variant-as-name (analyzer.cpp:4639-4647). Same `can_be_builtin` gate; we always expose.
    if name == "Variant" {
        ctx.set_type(
            id,
            DataType {
                kind: DtKind::Variant,
                type_source: TypeSource::AnnotatedExplicit,
                is_meta_type: true,
                is_pseudo_type: true,
                ..Default::default()
            },
        );
    }

    // 7b. `@GlobalScope` enum value (e.g. `CLOCKWISE`). Resolves to the enum's instance
    // type so downstream `set_direction(CLOCKWISE)` arg-compat checks see ClockDirection,
    // not a soft Variant (which would false-positive UNSAFE_CALL_ARGUMENT). Mirrors
    // Godot's `GLOBAL_CONSTANT` arm at analyzer.cpp:4583-4599.
    if let Some((enum_name, val)) = ctx.native.global_enum_value(&name) {
        let dt = crate::resolver::make_global_enum_type(ctx, &enum_name, "", false);
        ctx.set_type(id, dt);
        ctx.folds.set(id, FoldedValue::Int(val));
        return;
    }

    // 7c. `@GlobalScope` enum NAME (e.g. `ClockDirection`). Resolves to the enum's meta
    // type so `ClockDirection.CLOCKWISE` subscript-attribute access reads the meta and
    // looks up `CLOCKWISE` as a value of the enum.
    if ctx.native.global_enum(&name).is_some() {
        let dt = crate::resolver::make_global_enum_type(ctx, &name, "", true);
        ctx.set_type(id, dt);
        return;
    }

    // 8. Global constants (PI, TAU, INF, NAN) — Godot's `GDScriptLanguage::get_global_map()`
    //    exposes these as global constants. Hard-code the set that appears in the corpus.
    if matches!(name.as_str(), "PI" | "TAU" | "INF" | "NAN") {
        ctx.set_type(
            id,
            DataType {
                type_source: TypeSource::AnnotatedExplicit,
                kind: DtKind::Builtin,
                builtin_type: VariantType::Float,
                is_constant: true,
                ..Default::default()
            },
        );
        return;
    }

    // 9. Autoload singleton → Script INSTANCE type (Godot `ScriptServer` singleton). Truly last
    //    fallback: only fires when every higher-priority lookup (local, param, class member,
    //    native, class_name, builtin, @GlobalScope enum, global constant) has missed. An autoload
    //    named like a builtin (`Color`) or a global enum therefore never shadows the language-level
    //    meaning; a member or local named the same as an autoload shadows the autoload (Godot's own
    //    precedence order: `ScriptServer::get_global_class` is checked before autoloads, but both
    //    are below in-scope identifiers — mirrored faithfully by placing this after step 5+6+7+8).
    //    This is ADDITIVE: it only resolves where nothing else resolved. It cannot affect the
    //    300/300 conformance corpus (which has no project.godot autoloads — `autoload_file` returns
    //    `None` there via the default impl).
    if let Some(fid) = ctx.xfile.autoload_file(&name) {
        let site = ctx.node(id).span;
        // Record a Use binding so `textDocument/references` and `textDocument/definition` on
        // the autoload name work. Class kind mirrors the `class_name` branch at step 5, since
        // an autoload is conceptually a class instance.
        ctx.record_binding(Binding::use_(
            Some(fid),
            BindingSymbolKind::Class,
            name.clone(),
            site,
        ));
        // Script INSTANCE type — unlike `script_meta_type` (the class_name metatype). The singleton
        // IS the instance; callers do `Global.method()`, not `Global.new()`. Reuse the existing
        // metatype→instance lowering (`type_from_metatype`) the class_name branch uses, so this stays
        // field-identical if those helpers change. This makes `reduce_identifier_from_base` /
        // `reduce_subscript` walk the script's interface members — the same path that resolves
        // `var l: Lib; l.helper()` (M6-E), which already works.
        ctx.set_type(id, type_from_metatype(script_meta_type(ctx, fid)));
        return;
    }

    // 9b. Utility function referenced as a first-class Callable (analyzer.cpp:4641-4652): a
    //    bare `print` / `len` / `floor` reduces to a constant Callable — `print.call_deferred(m)`,
    //    `arr.map(floor)`, `var f := absi`, `const PRINTER = print`. Godot's arm checks
    //    `Variant::has_utility_function || GDScriptUtilityFunctions::function_exists`; gdls
    //    mirrors with the NativeDb utility table (Variant utilities, extension_api.json) plus
    //    the hard-coded GDScript-only table (`gd_utility_return_type`). Godot also folds
    //    `Callable(memnew(GDScriptUtilityCallable(name)))` into `reduced_value` and types via
    //    `make_callable_type(method_info)`; gdls can't materialize a Callable value or carry a
    //    MethodInfo, so an `Opaque(Callable)` fold stands in for `reduced_value`: it makes a
    //    const initialized from a utility propagate constancy to that const's own references
    //    (the local/member Constant arms copy the initializer's fold), and it routes invalid
    //    operator use through Godot's reduced-operand template (`Invalid operands to operator
    //    +, Callable and int.` — the type-only tail would emit the wrong message). The
    //    constant-Callable shape matches the in-file member-function arm
    //    (`lookup_class_member`'s Function arm); `is_constant` fires the constant-assignment
    //    error for `print = 5` / `len = 5`, exactly as Godot. Skipped in callee position: a direct `print(x)`
    //    dispatches by name through `reduce_call`'s utility arms (analyzer.cpp:3481/3517 — the
    //    `utility_return` lookup below), and in Godot a direct call's identifier callee never
    //    reaches this arm at all. Placed after the autoload arm because Godot checks autoloads
    //    (analyzer.cpp:4570) before utilities (4641) — a project autoload named like a utility
    //    shadows it — and no utility name can collide with anything steps 4-8 resolve.
    if !ctx.reducing_callee
        && (ctx.native.utility(&name).is_some() || gd_utility_return_type(&name).is_some())
    {
        ctx.folds
            .set(id, FoldedValue::Opaque(VariantType::Callable));
        ctx.set_type(
            id,
            DataType {
                type_source: TypeSource::AnnotatedExplicit,
                kind: DtKind::Builtin,
                builtin_type: VariantType::Callable,
                is_constant: true,
                ..Default::default()
            },
        );
        return;
    }

    // 10. analyzer.cpp:4658-4660 — `Identifier "X" not declared in the current scope.` fires as
    //    the last fallthrough after all lookup paths are exhausted. gdls hasn't ported global
    //    enums, autoloads, or native properties fully, so we gate on: identifier has no type
    //    set, not a call callee, not self/super, not a plausible native member (walked via the
    //    class's native base chain), and not starting with an uppercase letter (likely a native
    //    class or global enum not in the trimmed DB).
    if !ctx.reducing_callee && name != "self" && name != "super" {
        let dt = ctx.get_type(id);
        if !dt.is_set() {
            let is_native_member = is_plausible_native_member(ctx, &name);
            let is_global_like = name.starts_with(|c: char| c.is_ascii_uppercase());
            if !is_native_member && !is_global_like {
                ctx.push_error(
                    format!(r#"Identifier "{name}" not declared in the current scope."#),
                    id,
                );
            }
        }
    }
}

/// The cross-file Script base at the bottom of the current class's in-file chain, if any —
/// the entry point for inherited-member lookups that must continue past the file boundary.
pub(crate) fn current_class_script_base(
    ctx: &AnalysisContext,
) -> Option<crate::data_type::ScriptRef> {
    script_base_of_class(ctx, ctx.current_class?)
}

/// The cross-file Script base at the bottom of `class_id`'s in-file base chain, if any.
fn script_base_of_class(
    ctx: &AnalysisContext,
    class_id: NodeId,
) -> Option<crate::data_type::ScriptRef> {
    let mut cur = class_id;
    loop {
        let base = ctx.bases.get(&cur).cloned().unwrap_or_default();
        match base.kind {
            DtKind::Script => return base.script_type,
            DtKind::Class => cur = base.class_node?,
            _ => return None,
        }
    }
}

fn is_plausible_native_member(ctx: &AnalysisContext, name: &str) -> bool {
    let Some(class_id) = ctx.current_class else {
        return false;
    };
    let mut cur = class_id;
    loop {
        let base = ctx.bases.get(&cur).cloned().unwrap_or_default();
        match base.kind {
            DtKind::Native => return native_member_plausible(ctx, &base.native_type, name),
            DtKind::Class => match base.class_node {
                Some(c) => cur = c,
                None => return false,
            },
            DtKind::Script => {
                // Cross-file base: an inherited script member (any kind) makes the name
                // plausible, then the chain's native root continues the native probe — the
                // direct analog of Godot's script_classes walk + ClassDB tail
                // (analyzer.cpp:4166-4360). An unknown chain treats everything as plausible:
                // permissive, never `Identifier "x" not declared` against a base we can't see.
                let Some(sr) = base.script_type.as_ref() else {
                    return true;
                };
                let chain = crate::script_chain::resolve_script_chain(ctx, sr);
                for link in &chain.links {
                    if let Some(iface) = crate::script_chain::link_interface(ctx.xfile, link) {
                        if iface.members.iter().any(|m| m.name == name) {
                            return true;
                        }
                    }
                }
                return match chain.native_root.as_deref() {
                    Some(root) => native_member_plausible(ctx, root, name),
                    None => true,
                };
            }
            _ => return false,
        }
    }
}

/// Whether `name` is a method, property, or signal anywhere up `native`'s inherits chain — the
/// native tail of [`is_plausible_native_member`], shared by the in-file and cross-file base
/// arms. Signals matter: bare `changed.connect(...)` / `ready` against an inherited native
/// signal is everyday GDScript and must never read as undeclared.
fn native_member_plausible(ctx: &AnalysisContext, native: &str, name: &str) -> bool {
    let mut native = Some(native.to_owned());
    while let Some(c) = native {
        let Some(nc) = ctx.native.class_named(&c) else {
            break;
        };
        if nc
            .methods
            .iter()
            .any(|m| ctx.native.name_of(m.name) == name)
        {
            return true;
        }
        if nc
            .properties
            .iter()
            .any(|p| ctx.native.name_of(p.name) == name)
        {
            return true;
        }
        if nc
            .signals
            .iter()
            .any(|s| ctx.native.name_of(s.name) == name)
        {
            return true;
        }
        native = nc.inherits.map(|s| ctx.native.name_of(s).to_owned());
    }
    false
}

// --- reduce_identifier helpers ---------------------------------------------------------------------

/// Which non-static instance member kind matched a name during a static-context check.
/// Mirrors Godot's three `source_is_*` branches at analyzer.cpp:4464-4490.
#[derive(Clone, Copy)]
enum NonStaticKind {
    Variable,
    Function,
    Signal,
}

/// Walk the same scope as [`lookup_class_member`] but return the **member kind** if it's a
/// non-static instance member (Variable / non-static Function / Signal). Returns `None` for
/// static functions, constants, enums, classes, etc. — none of those need the static-context
/// check. Used by `reduce_identifier` to fire
/// `Cannot access non-static <kind> "X" from the static function "Y()".`.
fn non_static_instance_member_kind(
    ctx: &AnalysisContext,
    class_id: NodeId,
    name: &str,
) -> Option<NonStaticKind> {
    use gd_syntax::ast::Member;
    for class in crate::resolver::scope_classes(ctx, class_id) {
        let member = match &ctx.node(class).kind {
            NodeKind::Class(c) => c
                .members_indices
                .get(name)
                .and_then(|&idx| c.members.get(idx).cloned()),
            _ => None,
        };
        if let Some(m) = member {
            return match m {
                Member::Variable(vid) => match &ctx.node(vid).kind {
                    NodeKind::Variable(v) if !v.is_static => Some(NonStaticKind::Variable),
                    _ => None,
                },
                Member::Signal(_) => Some(NonStaticKind::Signal),
                Member::Function(fid) => match &ctx.node(fid).kind {
                    NodeKind::Function(f) if !f.is_static => Some(NonStaticKind::Function),
                    _ => None,
                },
                _ => None,
            };
        }
    }
    None
}

/// The name of the enclosing **concrete** function (skipping lambdas), or `None` if we're at
/// class-level (e.g. a static-variable initializer). Mirrors Godot's `source_lambda`
/// parent walk at analyzer.cpp:4471-4474 — gdls doesn't yet carry a back-pointer from a
/// lambda's `FunctionNode` to the lambda expression that owns it (lambda bodies aren't
/// entered, so the walk is implicit: `ctx.current_function` is already the concrete
/// enclosing function, never a lambda). Once lambda-body resolution lands, this becomes
/// the explicit `while parent.source_lambda { parent = parent.source_lambda.parent }` walk.
fn enclosing_concrete_function_name(ctx: &AnalysisContext) -> Option<String> {
    // analyzer.cpp:3645-3649 — Godot walks `source_lambda -> parent_function` up the lambda
    // chain to reach the enclosing concrete function. gdls's AST doesn't carry those back-pointers;
    // instead `ctx.concrete_function` snapshots the outer regular function whenever a lambda body
    // is entered. Fall back to `current_function` when no lambda is in play.
    let fn_id = ctx.concrete_function.or(ctx.current_function)?;
    let ident_id = match &ctx.node(fn_id).kind {
        NodeKind::Function(f) => f.identifier?,
        _ => return None,
    };
    match &ctx.node(ident_id).kind {
        NodeKind::Identifier(i) => Some(i.name.clone()),
        _ => None,
    }
}

/// Walk [`AnalysisContext::suite_stack`] innermost-first for a local named `name` (the analog of
/// Godot's `SuiteNode::has_local` chain reached via `IdentifierNode::suite`).
pub(crate) fn lookup_local(ctx: &AnalysisContext, name: &str) -> Option<gd_syntax::ast::Local> {
    for &suite_id in ctx.suite_stack.iter().rev() {
        if let NodeKind::Suite(s) = &ctx.node(suite_id).kind {
            if let Some(&idx) = s.locals_indices.get(name) {
                if let Some(local) = s.locals.get(idx) {
                    return Some(local.clone());
                }
            }
        }
    }
    None
}

/// The `initializer` field of a `Constant` node (used to fish the folded value for local-const
/// identifier lookups, analyzer.cpp:4402).
fn constant_initializer_of(ctx: &AnalysisContext, id: NodeId) -> Option<NodeId> {
    match &ctx.node(id).kind {
        NodeKind::Constant(c) => c.initializer,
        _ => None,
    }
}

/// A function's parameter named `name`, if any — Godot's parser pre-fills
/// `IdentifierNode::parameter_source` (gdscript_parser.cpp:3084) and the analyzer reaches it via
/// the back-pointer; gdls re-derives it from `FunctionNode::parameters_indices`.
fn function_param_named(ctx: &AnalysisContext, fn_id: NodeId, name: &str) -> Option<NodeId> {
    if let NodeKind::Function(f) = &ctx.node(fn_id).kind {
        if let Some(&idx) = f.parameters_indices.get(name) {
            return f.parameters.get(idx).copied();
        }
    }
    None
}

/// Walk `class_id`'s members + the in-file `ctx.bases` chain looking for `name`. Returns the
/// member's resolved type + any folded constant value (for constants). Godot's
/// `reduce_identifier_from_base` (analyzer.cpp:4024) does this through native introspection +
/// `ClassNode::get_member`; this E3 port handles in-file members only — native-class members,
/// inherited script-class members across files, and the inheritance auto-resolution land later in
/// E3 with the rest of the cross-file machinery.
///
/// When the member's type is `Unresolved`, [`crate::resolver::resolve_class_member_by_name`] is
/// invoked **with `source = identifier_id`** so a cyclic reference detected via the RESOLVING
/// sentinel (analyzer.cpp:984-987) anchors at the referring identifier — matching the corpus
/// expectation for `cyclic_ref_var.gd` / `cyclic_ref_const.gd` etc. where the diagnostic line is
/// the reference line, not the declaration line. Godot's parser pre-tags `IdentifierNode` with
/// the declaration back-pointer; gdls re-derives the dependency edge here, which both feeds the
/// cycle detection and lazily forces the dependent member's resolution.
fn lookup_class_member(
    ctx: &mut AnalysisContext,
    class_id: NodeId,
    name: &str,
    identifier_id: NodeId,
) -> Option<(DataType, Option<FoldedValue>)> {
    // analyzer.cpp:4450-4455 — Godot's `reduce_identifier` walks the *full* scope
    // (`get_class_node_current_scope_classes`): the class, its inheritance chain, and its outer
    // chain. gdls previously only walked the inheritance chain (`ctx.bases`), so an identifier
    // referencing an enum/class declared on the *outer* class (e.g. `MyEnum.V1` from inside an
    // inner class's method, when both outer and inner declare a `MyEnum`) resolved to Variant
    // instead of the lexically-closest enum.
    let scope = crate::resolver::scope_classes(ctx, class_id);
    for class in scope {
        // analyzer.cpp:4161-4167 — the class itself is in scope under its own `class_name`.
        // gdls's `xfile.global_class_file` path only fires for cross-file lookups (and is
        // inert under `NoCrossFile`); the in-file case has to match here so a reference to
        // the head's `class_name` (e.g. `OuterClassName.InnerClass.Enum` inside any class
        // body) resolves to the head's meta type without round-tripping through the
        // cross-file query.
        let class_ident_match = match &ctx.node(class).kind {
            NodeKind::Class(c) => c
                .identifier
                .and_then(|iid| match &ctx.node(iid).kind {
                    NodeKind::Identifier(i) => Some(i.name.as_str() == name),
                    _ => None,
                })
                .unwrap_or(false),
            _ => false,
        };
        if class_ident_match {
            return Some((ctx.get_type(class).clone(), None));
        }
        let member = match &ctx.node(class).kind {
            NodeKind::Class(c) => c
                .members_indices
                .get(name)
                .and_then(|&idx| c.members.get(idx).cloned()),
            _ => None,
        };
        if let Some(m) = member {
            // Trigger lazy member resolution **unconditionally** for members other than the
            // function/class arms (analyzer.cpp:4175 calls `resolve_class_member(name, source)`
            // every time, letting the function's own top-guard decide between
            // already-set/resolving=cycle/unresolved). Function members don't recurse into a body
            // here (Godot's parser back-pointer would have tagged them as `MEMBER_FUNCTION`),
            // but resolving their signature is idempotent so it's safe.
            let target_node = match &m {
                gd_syntax::ast::Member::Variable(id)
                | gd_syntax::ast::Member::Constant(id)
                | gd_syntax::ast::Member::Signal(id)
                | gd_syntax::ast::Member::Enum(id)
                | gd_syntax::ast::Member::Function(id)
                | gd_syntax::ast::Member::Class(id) => Some(*id),
                gd_syntax::ast::Member::EnumValue(ev) => ev.identifier,
                gd_syntax::ast::Member::Group(_) => None,
            };
            if target_node.is_some() {
                crate::resolver::resolve_class_member_by_name(ctx, class, name, identifier_id);
            }
            return match m {
                gd_syntax::ast::Member::Variable(vid)
                | gd_syntax::ast::Member::Signal(vid)
                | gd_syntax::ast::Member::Class(vid)
                | gd_syntax::ast::Member::Enum(vid) => Some((ctx.get_type(vid).clone(), None)),
                // analyzer.cpp:4225 — referencing an in-file member function as a value
                // yields a constant Callable (via `make_callable_type(member.function->info)`).
                // The `is_constant` flag is what makes `function = 25` fire `Cannot assign a
                // new value to a constant.` (analyzer.cpp:2911-2912) for
                // `errors/function_used_as_property.gd`. The full MethodInfo wiring lives in
                // the make_callable_type slice; the constant-Callable shape on its own
                // suffices for the assignment-rejection arm.
                gd_syntax::ast::Member::Function(_) => Some((
                    DataType {
                        type_source: TypeSource::AnnotatedExplicit,
                        kind: DtKind::Builtin,
                        builtin_type: VariantType::Callable,
                        is_constant: true,
                        ..Default::default()
                    },
                    None,
                )),
                gd_syntax::ast::Member::Constant(cid) => {
                    let dt = ctx.get_type(cid).clone();
                    let fold = constant_initializer_of(ctx, cid)
                        .and_then(|init| ctx.folds.get(init).cloned());
                    Some((dt, fold))
                }
                gd_syntax::ast::Member::EnumValue(ev) => ev
                    .identifier
                    .map(|iid| (ctx.get_type(iid).clone(), ctx.folds.get(iid).cloned())),
                gd_syntax::ast::Member::Group(_) => None,
            };
        }
    }
    None
}

/// Build a native-class metatype, the result of `Identifier "Node"` (analyzer.cpp:4543).
fn native_meta_type(name: String) -> DataType {
    DataType {
        type_source: TypeSource::AnnotatedExplicit,
        kind: DtKind::Native,
        builtin_type: VariantType::Object,
        native_type: name,
        is_meta_type: true,
        is_constant: true,
        ..Default::default()
    }
}

/// Build a builtin-type metatype, the result of `Identifier "int"` (analyzer.cpp:4534).
fn builtin_meta_type(t: VariantType) -> DataType {
    DataType {
        type_source: TypeSource::AnnotatedExplicit,
        kind: DtKind::Builtin,
        builtin_type: t,
        is_meta_type: true,
        is_constant: true,
        ..Default::default()
    }
}

/// Build a script-file metatype, the result of `Identifier "MyClass"` where `MyClass` is a project
/// `class_name` (analyzer.cpp:4548). Carries the chain's native root like every Script type
/// (analyzer.cpp:617-619 propagation).
fn script_meta_type(ctx: &AnalysisContext, file: gd_project::FileId) -> DataType {
    let sref = crate::data_type::ScriptRef {
        file,
        inner: Vec::new(),
    };
    DataType {
        type_source: TypeSource::AnnotatedExplicit,
        kind: DtKind::Script,
        builtin_type: VariantType::Object,
        is_meta_type: true,
        is_constant: true,
        native_type: crate::script_chain::chain_native_root(ctx, &sref).unwrap_or_default(),
        script_type: Some(sref),
        ..Default::default()
    }
}

// ===================================================================================================
// reduce_cast — analyzer.cpp:3764
// ===================================================================================================

/// `GDScriptAnalyzer::reduce_cast` (analyzer.cpp:3764). E3c ports the structural skeleton + the
/// validity check that emits `Invalid cast. Cannot convert from "%s" to "%s".` (analyzer.cpp:3813),
/// using [`is_type_compatible`] for the non-builtin case and
/// [`data_type::variant_can_convert`] for the Builtin↔Builtin case (analyzer.cpp:3806-3807). The
/// constant-fold-propagation paths at analyzer.cpp:3775-3790
/// (`update_const_expression_builtin_type` / `update_array_literal_element_type` /
/// `update_dictionary_literal_element_type`) need the typed-collection family and land in a later
/// E3 slice; the `UNSAFE_CAST` warning emission at analyzer.cpp:3797 joins with WP-F.
fn reduce_cast(ctx: &mut AnalysisContext, id: NodeId) {
    let cast = match ctx.node(id).kind.clone() {
        NodeKind::Cast(c) => c,
        _ => return,
    };

    if let Some(operand) = cast.operand {
        reduce_expression(ctx, operand, false);
    }

    let cast_type = if cast.cast_type.is_some() {
        crate::resolver::type_from_metatype(crate::resolver::resolve_datatype(ctx, cast.cast_type))
    } else {
        return;
    };

    if !cast_type.is_set() {
        return; // analyzer.cpp:3769-3772 — unresolvable cast target, mark unsafe + bail.
    }

    ctx.set_type(id, cast_type.clone());

    // Validity check (analyzer.cpp:3792-3815). We skip when the cast target is itself Variant —
    // anything → Variant is always legal.
    if cast_type.is_variant() {
        return;
    }
    let Some(operand_id) = cast.operand else {
        return;
    };
    let op_type = ctx.get_type(operand_id).clone();
    if op_type.is_variant() || !op_type.is_hard_type() {
        // analyzer.cpp:3794-3798 — operand is Variant or soft-typed; Godot emits
        // `UNSAFE_CAST` with `cast_type.to_string()` as the lone symbol. No hard error.
        //
        // **Suppression for `$Node`/`%Unique`/`get_node()` operands** — docs/02-frontend-port.md
        // §10. gdls's permissive deferred-node policy types those expressions as Variant; in
        // Godot they're typed as Node, so Godot doesn't fire UNSAFE_CAST on `$Foo as Bar`.
        // Suppress to match. The corpus's `features/allow_get_node_with_onready.gd` exercises
        // this exact pattern.
        let operand_is_get_node = match &ctx.node(operand_id).kind {
            NodeKind::GetNode(_) => true,
            NodeKind::Call(c) => {
                matches!(c.function_name.as_str(), "get_node" | "get_node_or_null")
            }
            _ => false,
        };
        if !operand_is_get_node {
            ctx.push_warning(
                crate::warnings::WarningCode::UnsafeCast,
                &[cast_type.to_string()],
                id,
            );
        }
        return;
    }

    // analyzer.cpp:2744 — `INT_AS_ENUM_WITHOUT_MATCH`. When the operand folds to a specific
    // integer literal and the cast target is an enum whose members don't include that value,
    // Godot warns `Cannot cast <value> as Enum "<file.gd.Name>": no enum member has matching value.`.
    // Godot raises this inside `update_const_expression_builtin_type` with `p_is_cast=true`;
    // gdls's `reduce_cast` has its own validity matrix below, so the check is inlined here.
    if cast_type.kind == DtKind::Enum
        && !cast_type.enum_values_inexact
        && (op_type.builtin_type == VariantType::Int || op_type.kind == DtKind::Enum)
    {
        if let Some(crate::foldtable::FoldedValue::Int(v)) = ctx.folds.get(operand_id).cloned() {
            if !cast_type.enum_values.values().any(|&val| val == v) {
                ctx.push_warning(
                    crate::warnings::WarningCode::IntAsEnumWithoutMatch,
                    &["cast".to_owned(), v.to_string(), cast_type.to_string()],
                    id,
                );
            }
        }
    }

    // analyzer.cpp:3800-3810 — the four mutually-exclusive arms covering Int↔Enum widening,
    // Builtin↔Builtin (via Variant::can_convert), and non-Builtin↔non-Builtin (bidirectional
    // `is_type_compatible`). The Int→Enum and Enum→Int arms are both valid by definition (the
    // shared block matches Godot's two distinct branches at :3801 and :3804); collapsing them
    // into one match arm is the rustfmt/clippy-clean form.
    let int_enum_widen = (op_type.builtin_type == VariantType::Int
        && cast_type.kind == DtKind::Enum)
        || (op_type.kind == DtKind::Enum && cast_type.builtin_type == VariantType::Int);
    let valid = if int_enum_widen {
        true
    } else if op_type.kind == DtKind::Builtin && cast_type.kind == DtKind::Builtin {
        data_type::variant_can_convert(op_type.builtin_type, cast_type.builtin_type)
    } else if op_type.kind != DtKind::Builtin && cast_type.kind != DtKind::Builtin {
        // analyzer.cpp:3808-3810: bidirectional compatibility on object-family types.
        is_type_compatible(ctx, &cast_type, &op_type, false)
            || is_type_compatible(ctx, &op_type, &cast_type, false)
    } else {
        false // one is Builtin, the other isn't — never compatible (analyzer.cpp:3812).
    };

    if !valid {
        let cast_type_anchor = cast.cast_type.unwrap_or(id);
        // Humanized rendering: through v1.0.2 this message leaked the `Display` impl's
        // `<Script #N>` placeholder onto real projects (the v1.0.3 acceptance sweep caught
        // `Invalid cast. Cannot convert from "Nil" to "<Script #3095>".`).
        let from_str = script_type_display(ctx, &op_type);
        let to_str = script_type_display(ctx, &cast_type);
        ctx.push_error(
            format!(r#"Invalid cast. Cannot convert from "{from_str}" to "{to_str}"."#),
            cast_type_anchor,
        );
    }
}

/// Render a `DataType` the way Godot's `DataType::to_string()` does for user-facing messages:
/// a `Script`-kind type shows its global `class_name` when declared, else the script file's
/// basename (`script_path.get_file()`), with any inner-class path appended; `Class` kinds
/// delegate to [`class_identifier_name_or_default`]. The bare `Display` impl has no index
/// access, so it renders the `<Script #N>` / `<Class>` diagnostic placeholders — fine for
/// internal logs, never for diagnostics text.
pub(crate) fn script_type_display(ctx: &AnalysisContext, dt: &DataType) -> String {
    if dt.kind != DtKind::Script {
        return class_identifier_name_or_default(ctx, dt);
    }
    let Some(sr) = &dt.script_type else {
        return dt.to_string();
    };
    let head = ctx
        .xfile
        .interface(sr.file)
        .and_then(|i| i.class_name.clone())
        .or_else(|| {
            ctx.xfile
                .file_path(sr.file)
                .map(|p| p.rsplit(['/', '\\']).next().unwrap_or(p).to_owned())
                .filter(|s| !s.is_empty())
        });
    match head {
        Some(h) if sr.inner.is_empty() => h,
        Some(h) => format!("{h}.{}", sr.inner.join(".")),
        None => dt.to_string(),
    }
}

// ===================================================================================================
// is_type_compatible — analyzer.cpp:6281 / check_type_compatibility:6310
// ===================================================================================================

/// `GDScriptAnalyzer::is_type_compatible(target, source, allow_implicit_conversion)`
/// (analyzer.cpp:6281, delegating to `check_type_compatibility` at :6310). E3c ports the algorithm
/// for `Variant`, `Builtin`, `Native`, `Enum`, and `Script`/`Class` (the polymorphism walk uses
/// [`NativeDb`]'s `is_parent_class` and the in-file `ctx.bases` chain). The `INT_AS_ENUM_WITHOUT_CAST`
/// warning emission at analyzer.cpp:6284 joins with WP-F.
///
/// [`is_type_compatible_strict_collections`] sits one layer above this and rejects typed-collection
/// → untyped-collection assignments that this routine would otherwise accept.
/// [`is_type_compatible`] **with a source node** — upstream's warning-emitting wrapper
/// (analyzer.cpp:6139-6150): when the target is an enum and the source a builtin `int`,
/// INT_AS_ENUM_WITHOUT_CAST fires at the node, independent of the compatibility verdict (the
/// assignment may well compile — the cast is just missing). Only the call sites whose upstream
/// counterpart passes `p_source_node` route through here; node-less upstream calls (ternary,
/// type-test, cast, call arguments) keep the plain function and never warn.
pub(crate) fn is_type_compatible_with_source(
    ctx: &mut AnalysisContext,
    target: &DataType,
    source: &DataType,
    allow_implicit_conversion: bool,
    source_node: NodeId,
) -> bool {
    if target.kind == DtKind::Enum
        && source.kind == DtKind::Builtin
        && source.builtin_type == VariantType::Int
    {
        ctx.push_warning(
            crate::warnings::WarningCode::IntAsEnumWithoutCast,
            &[],
            source_node,
        );
    }
    is_type_compatible(ctx, target, source, allow_implicit_conversion)
}

pub(crate) fn is_type_compatible(
    ctx: &AnalysisContext,
    target: &DataType,
    source: &DataType,
    allow_implicit_conversion: bool,
) -> bool {
    // analyzer.cpp:6311 — `ERR_FAIL_COND_V_MSG(!is_set, true, …)`. Godot returns true defensively
    // so that an unset target doesn't cascade into false-positive errors.
    if !target.is_set() || !source.is_set() {
        return true;
    }
    // analyzer.cpp:6315-6323: Variant accepts/produces anything.
    if target.kind == DtKind::Variant || source.kind == DtKind::Variant {
        return true;
    }

    if target.kind == DtKind::Builtin {
        // analyzer.cpp:6325-6349 — Builtin↔Builtin. Uses STRICT can_convert
        // (`Variant::can_convert_strict` at variant.cpp:535), not the lenient runtime table — so
        // String ↛ Int / Float / Bool here, even though `Variant::convert` would allow it at
        // runtime.
        let mut valid =
            source.kind == DtKind::Builtin && target.builtin_type == source.builtin_type;
        if !valid && allow_implicit_conversion {
            // Godot's check has NO kind gate (analyzer.cpp:6328): it reads `builtin_type`
            // directly, which is OBJECT for Native/Script/Class sources — that is what lets an
            // Object-derived argument satisfy an RID parameter (`can_convert_strict(OBJECT, RID)`
            // is registered, variant.cpp:850's table).
            valid = data_type::variant_can_convert_strict(source.builtin_type, target.builtin_type);
        }
        // analyzer.cpp:6330-6333 — Enum value compatible with Int.
        if !valid
            && target.builtin_type == VariantType::Int
            && source.kind == DtKind::Enum
            && !source.is_meta_type
        {
            valid = true;
        }
        // analyzer.cpp:6334-6339 — typed-Array narrowing. When BOTH operands have a typed
        // element, the element types must match exactly.
        if valid
            && target.builtin_type == VariantType::Array
            && source.builtin_type == VariantType::Array
            && !target.container_element_types.is_empty()
            && !source.container_element_types.is_empty()
            && target.container_element_types[0] != source.container_element_types[0]
        {
            valid = false;
        }
        // analyzer.cpp:6340-6348 — typed-Dictionary narrowing on both key and value.
        if valid
            && target.builtin_type == VariantType::Dictionary
            && source.builtin_type == VariantType::Dictionary
        {
            if !target.container_element_types.is_empty()
                && !source.container_element_types.is_empty()
                && target.container_element_types[0] != source.container_element_types[0]
            {
                valid = false;
            }
            if valid
                && target.container_element_types.len() >= 2
                && source.container_element_types.len() >= 2
                && target.container_element_types[1] != source.container_element_types[1]
            {
                valid = false;
            }
        }
        return valid;
    }

    if target.kind == DtKind::Enum {
        // analyzer.cpp:6352-6362.
        if source.kind == DtKind::Builtin && source.builtin_type == VariantType::Int {
            return true;
        }
        if source.kind == DtKind::Enum && source.native_type == target.native_type {
            return true;
        }
        return false;
    }

    // analyzer.cpp:6366-6369 — null is acceptable as an object reference.
    if source.kind == DtKind::Builtin && source.builtin_type == VariantType::Nil {
        return true;
    }

    // Same-file identity bridge: a Script type whose ref points at THIS file is the in-file
    // class by another name (annotations resolved through interfaces — e.g. an `Array[Own]`
    // element on another file's member — produce ScriptRefs even for this file's own classes;
    // `self` is Class-kind). Normalize to Class-kind so the node-identity arms below compare
    // them as the same class instead of erroring `"<Class>" vs "<Script #N>"`.
    let target_bridge;
    let target = match script_ref_as_in_file_class(ctx, target) {
        Some(t) => {
            target_bridge = t;
            &target_bridge
        }
        None => target,
    };
    let source_bridge;
    let source = match script_ref_as_in_file_class(ctx, source) {
        Some(s) => {
            source_bridge = s;
            &source_bridge
        }
        None => source,
    };

    // Polymorphism for object types — Godot's source decomposition + target switch
    // (`check_type_compatibility`'s object half, analyzer.cpp:6210-6296). The SOURCE decomposes
    // into (native root, script ref, class node); the TARGET kind then picks which of those to
    // compare. Cross-file Script chains resolve through `crate::script_chain`; an INCOMPLETE
    // chain (native_root unknown) makes the verdict permissively `true` — a deliberate,
    // documented deviation from Godot (whose ClassDB always bottoms out): an unresolvable chain
    // must never manufacture an "incompatible" error. In-file-only degenerate chains keep the
    // strict `false` they had before this decomposition.

    // --- Source decomposition (analyzer.cpp:6221-6258) ---------------------------------------
    let mut src_native: Option<String> = None;
    let mut src_script: Option<crate::data_type::ScriptRef> = None;
    let mut src_chain_unknown = false;
    match source.kind {
        DtKind::Native => {
            // analyzer.cpp:6221-6231 — a native source can only satisfy a Native target.
            if target.kind != DtKind::Native {
                return false;
            }
            src_native = Some(source.native_type.clone());
        }
        DtKind::Script => {
            if source.is_meta_type {
                // A script META is an engine `Script` object (analyzer.cpp:6233-6236). The
                // trimmed DB may not carry the `Script` class, so stay permissive — the
                // pre-decomposition code returned `true` for every Script operand.
                return true;
            }
            let Some(sr) = source.script_type.as_ref() else {
                return true; // degenerate Script type — permissive
            };
            src_script = Some(sr.clone());
            match crate::script_chain::chain_native_root(ctx, sr) {
                Some(root) => src_native = Some(root),
                None => src_chain_unknown = true,
            }
        }
        DtKind::Class => {
            // Walk the in-file base chain to the bottom (analyzer.cpp:6246-6258); a Script
            // bottom continues through the cross-file chain — that link is what makes
            // `f(self)` against a cross-file base class compatible.
            let mut cur = source.class_node;
            let mut hops = 0usize;
            while let Some(n) = cur {
                hops += 1;
                if hops > 256 {
                    break; // defensive: malformed in-file cycle
                }
                let base = ctx.bases.get(&n).cloned().unwrap_or_default();
                match base.kind {
                    DtKind::Native => {
                        src_native = Some(base.native_type.clone());
                        break;
                    }
                    DtKind::Script => {
                        if let Some(sr) = base.script_type.as_ref() {
                            src_script = Some(sr.clone());
                            if !base.native_type.is_empty() {
                                src_native = Some(base.native_type.clone());
                            } else {
                                match crate::script_chain::chain_native_root(ctx, sr) {
                                    Some(root) => src_native = Some(root),
                                    None => src_chain_unknown = true,
                                }
                            }
                        } else {
                            src_chain_unknown = true;
                        }
                        break;
                    }
                    DtKind::Class => cur = base.class_node,
                    _ => break, // unresolved in-file base — keep the strict legacy verdict
                }
            }
        }
        _ => return false,
    }

    // --- Target switch (analyzer.cpp:6266-6296) ----------------------------------------------
    match target.kind {
        DtKind::Native => {
            if src_chain_unknown {
                return true;
            }
            src_native
                .map(|n| ctx.native.is_subclass_of_named(&n, &target.native_type))
                .unwrap_or(false)
        }
        DtKind::Script => {
            // analyzer.cpp:6274-6284 — the `get_base_script()` loop: the source's script chain
            // must pass through the target's ScriptRef (file + inner identity).
            let Some(target_sr) = target.script_type.as_ref() else {
                return true; // degenerate Script target — permissive
            };
            let Some(start) = src_script else {
                return src_chain_unknown;
            };
            let chain = crate::script_chain::resolve_script_chain(ctx, &start);
            if chain.links.iter().any(|l| l == target_sr) {
                return true;
            }
            // Complete chain without a hit ⇒ genuinely incompatible; incomplete ⇒ permissive.
            chain.native_root.is_none()
        }
        DtKind::Class => {
            // analyzer.cpp:6285-6295 — node-identity walk, then the cross-file bridge: a Script
            // source whose extends CHAIN passes through THIS file's class satisfies the in-file
            // Class target (Godot compares fqcn through get_base_script(), 6291) — `self is
            // SubClassInOtherFile` / `node = node.parent` where parent's annotation resolved
            // cross-file back into this file's hierarchy.
            let target_node = match target.class_node {
                Some(n) => n,
                None => return false,
            };
            let mut cur = source.class_node;
            while let Some(n) = cur {
                if n == target_node {
                    return true;
                }
                cur = ctx.bases.get(&n).and_then(|b| b.class_node);
            }
            if let Some(start) = src_script {
                if let Some(target_sr) = in_file_script_ref_of_class(ctx, target_node) {
                    let chain = crate::script_chain::resolve_script_chain(ctx, &start);
                    if chain.links.contains(&target_sr) {
                        return true;
                    }
                    return chain.native_root.is_none(); // incomplete ⇒ permissive
                }
            }
            false
        }
        _ => false,
    }
}

/// The this-file [`ScriptRef`] identity of an in-file class NODE — root has an empty inner
/// path; inner classes get their name chain (depth-first search from the root). The inverse of
/// [`script_ref_as_in_file_class`].
fn in_file_script_ref_of_class(
    ctx: &AnalysisContext,
    node: NodeId,
) -> Option<crate::data_type::ScriptRef> {
    let file = ctx.file?;
    let root = ctx.tree.root_id()?;
    fn search(ctx: &AnalysisContext, cur: NodeId, target: NodeId, path: &mut Vec<String>) -> bool {
        if cur == target {
            return true;
        }
        let members = match &ctx.node(cur).kind {
            NodeKind::Class(c) => c.members.clone(),
            _ => return false,
        };
        for m in members {
            if let gd_syntax::ast::Member::Class(inner_id) = m {
                let name = match &ctx.node(inner_id).kind {
                    NodeKind::Class(c) => c
                        .identifier
                        .and_then(|i| match &ctx.node(i).kind {
                            NodeKind::Identifier(ident) => Some(ident.name.clone()),
                            _ => None,
                        })
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                path.push(name);
                if search(ctx, inner_id, target, path) {
                    return true;
                }
                path.pop();
            }
        }
        false
    }
    let mut path = Vec::new();
    if search(ctx, root, node, &mut path) {
        Some(crate::data_type::ScriptRef { file, inner: path })
    } else {
        None
    }
}

/// The in-file `Class` form of a Script-typed INSTANCE whose ref points at the CURRENT file —
/// `None` when it's another file's script (or a meta type). The bridge that makes `self` and a
/// same-class annotation resolved through interfaces compare equal.
fn script_ref_as_in_file_class(ctx: &AnalysisContext, dt: &DataType) -> Option<DataType> {
    if dt.kind != DtKind::Script || dt.is_meta_type {
        return None;
    }
    let sr = dt.script_type.as_ref()?;
    if Some(sr.file) != ctx.file {
        return None;
    }
    let mut node = ctx.tree.root_id()?;
    for seg in &sr.inner {
        node = crate::resolver::inner_class_named(ctx, node, seg)?;
    }
    let mut out = dt.clone();
    out.kind = DtKind::Class;
    out.class_node = Some(node);
    out.script_type = None;
    Some(out)
}

/// `GDScriptAnalyzer::is_type_compatible_strict_collections(target, source)` (analyzer.cpp:6296-
/// 6307). A one-way compat check that tightens the typed-collection asymmetry: a typed
/// `Array[T]` / `Dictionary[K, V]` target rejects an untyped `Array` / `Dictionary` source even
/// though the plain [`is_type_compatible`] would accept it (because the typed-collection narrowing
/// in `is_type_compatible` at analyzer.cpp:6334-6348 only fires when **both** sides have
/// container element types).
///
/// Used by:
/// * **`reduce_type_test`** (analyzer.cpp:5199, 5201) — constant-operand arm of the `is` operator;
///   makes `<untyped_array_const> is Array[int]` provably false rather than indeterminate.
/// * **`reduce_match_branch`** (analyzer.cpp:5545) — pattern-matching arm; same logic for the
///   `is`-shaped pattern check.
pub(crate) fn is_type_compatible_strict_collections(
    ctx: &AnalysisContext,
    target: &DataType,
    source: &DataType,
) -> bool {
    // analyzer.cpp:6297-6300 — typed Array target vs untyped Array source.
    if target.builtin_type == VariantType::Array && source.builtin_type == VariantType::Array {
        let target_has_elem = !target.container_element_types.is_empty();
        let source_has_elem = !source.container_element_types.is_empty();
        if target_has_elem && !source_has_elem {
            return false;
        }
    }
    // analyzer.cpp:6301-6304 — typed Dictionary target vs untyped Dictionary source. Godot's
    // `has_container_element_types()` returns true when **both** key + value containers are
    // present; we mirror that with `len() >= 2`.
    if target.builtin_type == VariantType::Dictionary
        && source.builtin_type == VariantType::Dictionary
    {
        let target_has_kv = target.container_element_types.len() >= 2;
        let source_has_kv = source.container_element_types.len() >= 2;
        if target_has_kv && !source_has_kv {
            return false;
        }
    }
    // analyzer.cpp:6306 — defer to plain is_type_compatible (allow_implicit_conversion = false,
    // matching Godot's default argument at gdscript_analyzer.h:286).
    is_type_compatible(ctx, target, source, false)
}

// ===================================================================================================
// reduce_self — analyzer.cpp:4759
// ===================================================================================================

/// `GDScriptAnalyzer::reduce_self` (analyzer.cpp:4759): `self` evaluates to an instance of the
/// current class. Without a current class (e.g. an expression in a top-level annotation),
/// Godot's `parser->current_class->get_datatype()` would dereference null; gdls degrades to `Variant`
/// (CLAUDE.md "never crash, never lie") and leaves the lambda-use-self bookkeeping for E3.
fn reduce_self(ctx: &mut AnalysisContext, id: NodeId) {
    let Some(cc) = ctx.current_class else {
        ctx.set_type(id, variant_dt());
        return;
    };
    let class_meta = ctx.get_type(cc).clone();
    ctx.set_type(id, type_from_metatype(class_meta));
}

// ===================================================================================================
// reduce_assignment — analyzer.cpp:2852
// ===================================================================================================

/// `GDScriptAnalyzer::reduce_assignment(p_assignment)` (analyzer.cpp:2852). Drives type-checking of
/// `assignee = value`, `assignee += value`, ... Builds on:
///
/// * **Constant-target detection** (analyzer.cpp:2911-2919): the assignee is a constant (a `const`
///   member, an enum value, a signal, ...) OR a subscript whose base is a non-Script-non-Class
///   constant — both emit `Cannot assign a new value to a constant.` (the second arm covers
///   `const arr: Array = [0]; arr[0] = 1`).
/// * **Read-only target detection** (analyzer.cpp:2920-2941): a read-only property (or a nested
///   subscript through a read-only base of a non-shared builtin type) — emits
///   `Cannot assign a new value to a read-only property.`
/// * **Constant-expression coercion** for hard-typed Builtin/Enum assignees with constant
///   right-hand side — calls [`update_const_expression_builtin_type`] which emits
///   `Cannot assign a value of type "X" as "Y".`
/// * **Compatibility matrix** (analyzer.cpp:2957-3031) — the gradual-typing variant/hard/op-typed
///   cross-checks. For `OP_NONE`, an incompatible hard-typed value triggers
///   `Value of type "X" cannot be assigned to a variable of type "Y".` For op-assign (`+=` /
///   `*=` / ...), `get_operation_type`'s compatibility report drives
///   `Invalid operands "X" and "Y" for assignment operator.`
///
/// The `mark_node_unsafe` / `use_conversion_assign` / `downgrade_node_type_source` plumbing
/// (analyzer.cpp:2962-3037) and the NARROWING_CONVERSION / UNASSIGNED_VARIABLE_OP_ASSIGN warnings
/// (analyzer.cpp:3041-3051) are WP-F items; the full `get_operation_type` operator matrix
/// (analyzer.cpp:6215) lands with the operator slice.
fn reduce_assignment(ctx: &mut AnalysisContext, id: NodeId) {
    let assign = match ctx.node(id).kind.clone() {
        NodeKind::Assignment(a) => a,
        _ => return,
    };

    // analyzer.cpp:2868-2902 — CONFUSABLE_CAPTURE_REASSIGNMENT. When the assignment runs
    // inside a lambda body (current_function != concrete_function — the lambda's FunctionNode
    // is pushed against the outer concrete) and the assignee is a plain identifier resolving
    // to a local declared OUTSIDE the lambda's body span, the reassignment doesn't escape
    // the lambda — warn the user. Subscript/attribute LHS (e.g. `dict.x = ...`,
    // `array[0] = ...`) doesn't fire because Godot only emits for the
    // "Reassigning lambda capture" template — by-reference container mutation is fine. Check
    // before reducing so the local's source position is unambiguous.
    if let Some(assignee_id) = assign.assignee {
        let in_lambda =
            ctx.current_function.is_some() && ctx.current_function != ctx.concrete_function;
        // Resolve a "candidate" base identifier name + whether this assignment shape can
        // warn for a captured value-type base. Two shapes warn:
        //  - plain identifier LHS  → always emit when the local is captured
        //  - subscript-attribute LHS on a value-type base → emit when the base local is
        //    captured (vector.x = ..., where vector is Vector2i; container types like
        //    Dictionary/Array stay silent — by-reference mutation propagates)
        let candidate: Option<(String, bool)> = if in_lambda {
            match ctx.node(assignee_id).kind.clone() {
                NodeKind::Identifier(i) => Some((i.name, false)),
                NodeKind::Subscript(s)
                    if matches!(
                        s.access,
                        Some(gd_syntax::ast::SubscriptAccess::Attribute(_))
                    ) =>
                {
                    s.base.and_then(|b| match ctx.node(b).kind.clone() {
                        NodeKind::Identifier(i) => Some((i.name, true)),
                        _ => None,
                    })
                }
                _ => None,
            }
        } else {
            None
        };
        if let Some((name, is_attr_on_base)) = candidate {
            let lambda_body_span = ctx
                .current_function
                .and_then(|fn_id| match &ctx.node(fn_id).kind {
                    NodeKind::Function(f) => f.body,
                    _ => None,
                })
                .map(|b| ctx.node(b).span);
            for &suite_id in ctx.suite_stack.clone().iter().rev() {
                if let NodeKind::Suite(s) = &ctx.node(suite_id).kind {
                    if let Some(&idx) = s.locals_indices.get(&name) {
                        if let Some(local) = s.locals.get(idx) {
                            let local_pos = ctx.node(local.source).span.start;
                            let outside_lambda = lambda_body_span
                                .is_some_and(|sp| local_pos < sp.start || local_pos >= sp.end);
                            // Lambda parameters lie outside the body span (declared at the
                            // function header) but are still local to the lambda — assigning
                            // to them isn't capture reassignment.
                            let is_param =
                                matches!(local.kind, gd_syntax::ast::LocalKind::Parameter);
                            if outside_lambda && !is_param {
                                let emit = if is_attr_on_base {
                                    // Only emit for value-type bases. Reference types
                                    // (Object / Array / Dictionary / Callable / Signal) propagate
                                    // mutation through the captured reference.
                                    let bt = ctx.get_type(local.source).clone();
                                    bt.kind == DtKind::Builtin
                                        && !matches!(
                                            bt.builtin_type,
                                            VariantType::Object
                                                | VariantType::Array
                                                | VariantType::Dictionary
                                                | VariantType::Callable
                                                | VariantType::Signal
                                                | VariantType::Nil
                                        )
                                } else {
                                    true
                                };
                                if emit {
                                    ctx.push_warning(
                                        crate::warnings::WarningCode::ConfusableCaptureReassignment,
                                        std::slice::from_ref(&name),
                                        id,
                                    );
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }
    }

    // analyzer.cpp:2853 + 2866 — reduce value first, then assignee (so e.g.
    // `var = func()` doesn't read the assignee's old type during the value's reduction).
    if let Some(v) = assign.assigned_value {
        reduce_expression(ctx, v, false);
    }
    // analyzer.cpp:2852-2860 — count the assignment for a local-variable assignee BEFORE
    // reducing the assignee, so the assignee's own read (`x` in `x = 1`) doesn't fire
    // UNASSIGNED_VARIABLE for the very assignment that initializes it.
    if let Some(a) = assign.assignee {
        if let NodeKind::Identifier(i) = &ctx.node(a).kind {
            let name = i.name.clone();
            if let Some(local) = lookup_local(ctx, &name) {
                if local.kind == gd_syntax::ast::LocalKind::Variable {
                    *ctx.assignments.entry(local.source).or_insert(0) += 1;
                }
            }
        }
    }
    if let Some(a) = assign.assignee {
        reduce_expression(ctx, a, false);
    }

    let (Some(assignee_id), Some(value_id)) = (assign.assignee, assign.assigned_value) else {
        // analyzer.cpp:2905-2907 — bail when either side is missing (parser-error path).
        return;
    };

    let assignee_type = ctx.get_type(assignee_id).clone();

    // analyzer.cpp:2911-2912 — `Cannot assign a new value to a constant.`
    if assignee_type.is_constant {
        ctx.push_error("Cannot assign a new value to a constant.", assignee_id);
        return;
    }

    // analyzer.cpp:2912-2918 — `arr[0] = 1` where `arr` is a constant. Godot gates on the base
    // EXPRESSION's reduced-constant flag (`base->is_constant`): a const collection is reduced,
    // while a native-class META like `Engine` is a type reference with no reduced value — so
    // `Engine.max_fps = x` is legal. gdls has no Array/Dictionary folds yet, so the DataType's
    // `is_constant` stands in for "reduced" with `!is_meta_type` carrying the type-reference
    // exclusion (metas are constant-typed but never reduced values).
    if let NodeKind::Subscript(s) = ctx.node(assignee_id).kind.clone() {
        if let Some(base) = s.base {
            let base_type = ctx.get_type(base).clone();
            if base_type.is_constant
                && !base_type.is_meta_type
                && base_type.kind != DtKind::Class
                && base_type.kind != DtKind::Script
            {
                ctx.push_error("Cannot assign a new value to a constant.", assignee_id);
                return;
            }
        }
    }

    // analyzer.cpp:2920-2941 — read-only property + nested-subscript read-only walk.
    if assignee_type.is_read_only {
        ctx.push_error(
            "Cannot assign a new value to a read-only property.",
            assignee_id,
        );
        return;
    }
    if let NodeKind::Subscript(_) = ctx.node(assignee_id).kind {
        let mut cur = Some(assignee_id);
        while let Some(sid) = cur {
            let NodeKind::Subscript(s) = ctx.node(sid).kind.clone() else {
                break;
            };
            let Some(base) = s.base else { break };
            let base_type = ctx.get_type(base).clone();
            if base_type.is_hard_type() && base_type.is_read_only {
                if base_type.kind == DtKind::Builtin && !builtin_is_shared(base_type.builtin_type) {
                    ctx.push_error(
                        "Cannot assign a new value to a read-only property.",
                        assignee_id,
                    );
                    return;
                }
            } else {
                break;
            }
            cur = if matches!(ctx.node(base).kind, NodeKind::Subscript(_)) {
                Some(base)
            } else {
                None
            };
        }
    }

    // analyzer.cpp:2944-2949 — when the assigned value is an array / dictionary literal AND the
    // assignee is a typed-container hard type, narrow the literal's element types so the
    // per-element check fires (`Cannot have an element of type "X" in an array of type "Array[Y]".`).
    if assignee_type.is_hard_type() {
        if let NodeKind::Array(_) = ctx.node(value_id).kind {
            if assignee_type.builtin_type == VariantType::Array
                && !assignee_type.container_element_types.is_empty()
            {
                let elem_t = assignee_type.container_element_types[0].clone();
                update_array_literal_element_type(ctx, value_id, &elem_t);
            }
        } else if let NodeKind::Dictionary(_) = ctx.node(value_id).kind {
            if assignee_type.builtin_type == VariantType::Dictionary
                && assignee_type.container_element_types.len() >= 2
            {
                let key_t = assignee_type.container_element_types[0].clone();
                let val_t = assignee_type.container_element_types[1].clone();
                update_dictionary_literal_element_type(ctx, value_id, &key_t, &val_t);
            }
        }
    }

    // analyzer.cpp:2951-2953 — coerce const constant expression to the assignee's hard type so
    // the "Cannot assign a value of type X as Y" arm fires alongside the compatibility check.
    let value_is_constant = ctx.folds.get(value_id).is_some();
    if assign.operation == gd_syntax::ast::AssignOp::None
        && assignee_type.is_hard_type()
        && value_is_constant
    {
        update_const_expression_builtin_type(ctx, value_id, &assignee_type, "assign");
    }

    let assigned_value_type = ctx.get_type(value_id).clone();
    let assignee_is_variant = assignee_type.is_variant();
    let assignee_is_hard = assignee_type.is_hard_type();
    let assigned_is_variant = assigned_value_type.is_variant();
    let mut compatible = true;
    let mut op_type = assigned_value_type.clone();

    if assign.operation != gd_syntax::ast::AssignOp::None && !op_type.is_variant() {
        // analyzer.cpp:2965-2986 — compound assignment routes through `get_operation_type` with the
        // operator-type matrix (variant_op.cpp), not a can_convert table. `bool += String` reads
        // as `OP_ADD Bool × String` which has no registration → `r_valid = false` → Godot
        // emits "Invalid operands ... for assignment operator." on the hard×hard×non-variant
        // path. The lenient `variant_can_convert` we used previously erroneously allowed the
        // String→Bool conversion (LENIENT table at variant.cpp:241 lists String among Bool's
        // convertibles), suppressing the error.
        let (computed_op_type, op_valid) =
            if let Some(binary_op) = binary_op_for_assign_op(assign.operation) {
                get_operation_type(binary_op, &assignee_type, &assigned_value_type)
            } else {
                (assigned_value_type.clone(), true)
            };
        compatible = op_valid;
        if assignee_is_variant {
            // mark unsafe (WP-F).
        } else if !compatible {
            // analyzer.cpp:2978-2980 — incompatible hard non-variant types.
            if !assigned_is_variant {
                ctx.push_error(
                    format!(
                        r#"Invalid operands "{assignee_type}" and "{assigned_value_type}" for assignment operator."#
                    ),
                    id,
                );
            }
        }
        // analyzer.cpp:2987 — the assignment's own datatype is `op_type` (the computed result of
        // the binary op), not the assignee. For now keep the assignee as the type record so the
        // downstream OP_NONE compatibility arm sees the declared shape; the assignment-node
        // datatype stamping joins with the full reduce_assignment slice.
        let _ = computed_op_type;
        op_type = assignee_type.clone();
    }

    // analyzer.cpp:3000-3031 — `OP_NONE` compatibility check. Only fire on the hard-non-variant
    // assignee + hard-incompatible-source case; soft/Variant operands stay silent here.
    if !assignee_is_variant
        && compatible
        && assign.operation == gd_syntax::ast::AssignOp::None
        // Upstream's forward check passes `p_assignment->assigned_value` (analyzer.cpp:3009) —
        // assigning a plain int to an enum-typed variable warns INT_AS_ENUM_WITHOUT_CAST.
        && !is_type_compatible_with_source(ctx, &assignee_type, &op_type, assignee_is_hard, value_id)
        && assignee_is_hard
        && !is_type_compatible(ctx, &op_type, &assignee_type, false)
    {
        // analyzer.cpp:3020 — `Value of type "X" cannot be assigned to a variable of type "Y".`
        ctx.push_error(
            format!(
                r#"Value of type "{assigned_value_type}" cannot be assigned to a variable of type "{assignee_type}"."#
            ),
            value_id,
        );
    }

    // analyzer.cpp:3041-3043 (`DEBUG_ENABLED`) — NARROWING_CONVERSION. When the assignee is a
    // hard-typed `int` and the value's builtin is `float`, precision is lost on the implicit
    // float→int conversion. Anchored on the assigned-value node so a
    // `@warning_ignore("narrowing_conversion")` on the enclosing statement / function
    // (per-line / region filter, WP-F3) suppresses correctly.
    if assignee_type.is_hard_type()
        && assignee_type.kind == DtKind::Builtin
        && assignee_type.builtin_type == VariantType::Int
        && assigned_value_type.kind == DtKind::Builtin
        && assigned_value_type.builtin_type == VariantType::Float
    {
        ctx.push_warning(
            crate::warnings::WarningCode::NarrowingConversion,
            &[],
            value_id,
        );
    }

    // analyzer.cpp:3043-3050 (`DEBUG_ENABLED`) — UNASSIGNED_VARIABLE_OP_ASSIGN: a compound
    // assignment to a local variable whose count is exactly the one this assignment added at
    // the head of this function ("Use == 1 here because this assignment was already counted
    // in the beginning of the function").
    if assign.operation != gd_syntax::ast::AssignOp::None {
        if let NodeKind::Identifier(i) = &ctx.node(assignee_id).kind {
            let name = i.name.clone();
            if let Some(local) = lookup_local(ctx, &name) {
                if local.kind == gd_syntax::ast::LocalKind::Variable
                    && assignment_count(ctx, local.source) == 1
                {
                    let op = assign_op_variant_name(assign.operation);
                    ctx.push_warning(
                        crate::warnings::WarningCode::UnassignedVariableOpAssign,
                        &[name, op.to_owned()],
                        id,
                    );
                }
            }
        }
    }

    ctx.set_type(id, op_type);
}

/// Godot's `VariableNode::assignments` total for a declaration: the lazily-folded initializer
/// contribution (gdscript_parser.cpp:1261 — `variable->assignments++` when an initializer is
/// parsed) plus the per-assignment increments `reduce_assignment` recorded on
/// [`AnalysisContext::assignments`].
pub(crate) fn assignment_count(ctx: &AnalysisContext, decl_id: NodeId) -> u32 {
    let initializer_bump = match &ctx.node(decl_id).kind {
        NodeKind::Variable(v) => u32::from(v.initializer.is_some()),
        _ => 0,
    };
    initializer_bump + ctx.assignments.get(&decl_id).copied().unwrap_or(0)
}

/// `Variant::get_operator_name` for the compound-assignment operators
/// (core/variant/variant_op.cpp's name table) — the `{1}` symbol in
/// UNASSIGNED_VARIABLE_OP_ASSIGN's message, which appends its own `=`.
fn assign_op_variant_name(op: gd_syntax::ast::AssignOp) -> &'static str {
    use gd_syntax::ast::AssignOp::*;
    match op {
        None => "",
        Addition => "+",
        Subtraction => "-",
        Multiplication => "*",
        Division => "/",
        Modulo => "%",
        Power => "**",
        BitShiftLeft => "<<",
        BitShiftRight => ">>",
        BitAnd => "&",
        BitOr => "|",
        BitXor => "^",
    }
}

/// Whether a builtin type is "shared" (analyzer.cpp:2928 wraps `Variant::is_type_shared`). Shared
/// types (Array, Dictionary, Object, ...) are pointer-like and don't trigger the
/// nested-read-only-subscript error on `state.center_of_mass.x +=`; non-shared types (Vector3,
/// Color, ...) do. Godot's table is at `core/variant/variant.cpp:230` (`Variant::is_type_shared`
/// — Array/Dictionary/Object/Callable/Signal/RID and the Packed*Array family are shared).
fn builtin_is_shared(t: VariantType) -> bool {
    matches!(
        t,
        VariantType::Array
            | VariantType::Dictionary
            | VariantType::Object
            | VariantType::Callable
            | VariantType::Signal
            | VariantType::Rid
            | VariantType::PackedByteArray
            | VariantType::PackedInt32Array
            | VariantType::PackedInt64Array
            | VariantType::PackedFloat32Array
            | VariantType::PackedFloat64Array
            | VariantType::PackedStringArray
            | VariantType::PackedVector2Array
            | VariantType::PackedVector3Array
            | VariantType::PackedColorArray
            | VariantType::PackedVector4Array
    )
}

// ===================================================================================================
// update_const_expression_builtin_type — analyzer.cpp:2722
// ===================================================================================================

/// `GDScriptAnalyzer::update_const_expression_builtin_type(p_expression, p_type, p_usage, p_is_cast)`
/// (analyzer.cpp:2722). When a constant expression is being passed where a hard-typed Builtin/Enum
/// target is expected, Godot:
///   1. Returns early when types are equal (analyzer.cpp:2723).
///   2. Returns early when the target isn't Builtin/Enum (analyzer.cpp:2726).
///   3. Emits `Cannot %s a value of type "X" as "Y".` when not type-compatible
///      (analyzer.cpp:2733). The `is_enum_cast` arm at :2731 narrows `int -> Enum` casts so
///      `arg as MyEnum` doesn't false-positive.
///
/// E3g ports steps 1-3 + the Variant-coerce path at :2737-:2740. The `Variant::construct`-based
/// value rewriting at :2754-:2760 (compile-time conversion) is deferred — gdls doesn't yet
/// evaluate constructors at analysis time.
pub(crate) fn update_const_expression_builtin_type(
    ctx: &mut AnalysisContext,
    expr_id: NodeId,
    p_type: &DataType,
    p_usage: &str,
) {
    let expression_type = ctx.get_type(expr_id).clone();
    // analyzer.cpp:2723 — types already equal: no-op.
    if expression_type.equiv(p_type) {
        return;
    }
    // analyzer.cpp:2726 — only Builtin/Enum target types take this path.
    if p_type.kind != DtKind::Builtin && p_type.kind != DtKind::Enum {
        return;
    }
    // analyzer.cpp:2731 — Int -> Enum cast is special-cased; the `p_is_cast` flag flips this on.
    // Callers in this slice pass `is_cast=false` (the reduce_cast path uses its own check), so we
    // don't need to wire it here; explicit casts go through `reduce_cast`'s validity matrix.

    // Upstream's call passes `p_expression` (analyzer.cpp:2729), so an int constant flowing
    // into an enum-typed slot warns INT_AS_ENUM_WITHOUT_CAST through the wrapper.
    if !is_type_compatible_with_source(ctx, p_type, &expression_type, true, expr_id) {
        ctx.push_error(
            format!(r#"Cannot {p_usage} a value of type "{expression_type}" as "{p_type}"."#),
            expr_id,
        );
        return;
    }

    // analyzer.cpp:2747-2751 — when the source VALUE's `builtin_type` already matches the
    // target's, the conversion is a no-op narrowing: stamp the target type onto the expression
    // and return. The comparison is on bare `builtin_type` with no kind gate, exactly as
    // upstream — that's what re-types an int constant flowing into an enum slot (enums carry
    // `builtin_type = INT`) so the caller's later compatibility check sees an enum source and
    // INT_AS_ENUM_WITHOUT_CAST fires exactly once, from this function's wrapper call.
    let value_type = ctx
        .folds
        .get(expr_id)
        .map(type_from_variant)
        .unwrap_or_else(|| expression_type.clone());
    if value_type.builtin_type == p_type.builtin_type {
        ctx.set_type(expr_id, p_type.clone());
    }

    // (When the builtin types differ:)
    // analyzer.cpp:2754-2770 — `Variant::construct(p_type.builtin_type, value)` runtime narrowing
    // for values whose builtin_type doesn't already match. Used for things like
    // `const X: int = 1.5` where the literal folds to 1.5 but the target is `int` → Godot
    // narrows the reduced value via `Variant::construct`. gdls's FoldedValue doesn't yet plumb
    // through Variant::construct (it'd need a full port of the runtime type system); the
    // arithmetic-folding paths in `reduce_unary_op` / `reduce_binary_op` produce typed
    // FoldedValues directly, which covers the corpus's `features/constant_expressions.gd`
    // cases. NARROWING_CONVERSION warning at analyzer.cpp:2764-2766 joins with the rest of
    // WP-F's narrowing family.
}

// ===================================================================================================
// update_array_literal_element_type — analyzer.cpp:2775
// ===================================================================================================

/// `GDScriptAnalyzer::update_array_literal_element_type(p_array, p_element_type)`
/// (analyzer.cpp:2775). When an array literal is stored (or passed) into a typed-Array context,
/// every element gets type-checked against the expected element type; mismatched hard-typed
/// elements emit `Cannot have an element of type "X" in an array of type "Array[Y]".` (and
/// foldable constants also trigger `update_const_expression_builtin_type` for the
/// `Cannot include a value of type "X" as "Y".` companion message).
///
/// On success the array's own `container_element_types[0]` is stamped with `expected`, so
/// `is_type_compatible_strict_collections` downstream sees `Array[T]` rather than bare `Array`.
pub(crate) fn update_array_literal_element_type(
    ctx: &mut AnalysisContext,
    array_id: NodeId,
    expected: &DataType,
) {
    // analyzer.cpp:2776-2777 — strip nested types (gdscript doesn't currently support
    // `Array[Array[int]]`); Godot's `container_element_types.clear()` removes them.
    let mut expected = expected.clone();
    expected.container_element_types.clear();

    let elements: Vec<NodeId> = match &ctx.node(array_id).kind {
        NodeKind::Array(a) => a.elements.clone(),
        _ => return,
    };

    for elem_id in elements {
        let is_const = ctx.folds.get(elem_id).is_some();
        if is_const {
            // analyzer.cpp:2781-2783 — emit the `Cannot include a value of type X as Y` companion
            // for constants.
            update_const_expression_builtin_type(ctx, elem_id, &expected, "include");
        }
        let actual = ctx.get_type(elem_id).clone();
        if actual.has_no_type() || actual.is_variant() || !actual.is_hard_type() {
            continue; // soft / Variant → mark_node_unsafe (WP-F warning), not an error.
        }
        // Upstream's forward check passes `p_array` (analyzer.cpp:2786) — the warning wrapper
        // anchors at the array literal, not the element; the reverse check is node-less.
        if !is_type_compatible_with_source(ctx, &expected, &actual, true, array_id)
            && !is_type_compatible(ctx, &actual, &expected, false)
        {
            // analyzer.cpp:2794 — `Cannot have an element of type "X" in an array of type
            // "Array[Y]".` Error is anchored at the offending element.
            ctx.push_error(
                format!(
                    r#"Cannot have an element of type "{actual}" in an array of type "Array[{expected}]"."#
                ),
                elem_id,
            );
            return;
        }
    }

    // analyzer.cpp:2799-2801 — stamp the literal's array DataType with the typed element.
    let mut arr_type = ctx.get_type(array_id).clone();
    if arr_type.container_element_types.is_empty() {
        arr_type.container_element_types.push(expected);
    } else {
        arr_type.container_element_types[0] = expected;
    }
    ctx.set_type(array_id, arr_type);
}

// ===================================================================================================
// update_dictionary_literal_element_type — analyzer.cpp:2806
// ===================================================================================================

/// `GDScriptAnalyzer::update_dictionary_literal_element_type(p_dict, p_key, p_value)`
/// (analyzer.cpp:2806). Mirrors `update_array_literal_element_type` for the key/value pair of a
/// `Dictionary[K, V]` typed context. Emits
/// `Cannot have a key of type "X" in a dictionary of type "Dictionary[K, V]".` and the
/// companion `Cannot have a value of type "X" in a dictionary of type "Dictionary[K, V]".`
pub(crate) fn update_dictionary_literal_element_type(
    ctx: &mut AnalysisContext,
    dict_id: NodeId,
    expected_key: &DataType,
    expected_value: &DataType,
) {
    let mut expected_key = expected_key.clone();
    let mut expected_value = expected_value.clone();
    expected_key.container_element_types.clear();
    expected_value.container_element_types.clear();

    let entries: Vec<gd_syntax::ast::KeyValue> = match &ctx.node(dict_id).kind {
        NodeKind::Dictionary(d) => d.elements.clone(),
        _ => return,
    };

    for kv in entries {
        if let Some(k) = kv.key {
            if ctx.folds.get(k).is_some() {
                update_const_expression_builtin_type(ctx, k, &expected_key, "include");
            }
            let actual = ctx.get_type(k).clone();
            // analyzer.cpp:2818 — Godot's `has_no_type || is_variant || !is_hard_type` guard
            // de-Morgan'd to a positive `is_hard_type && !is_variant && !has_no_type`.
            // Upstream's forward check passes `p_dictionary` (analyzer.cpp:2817).
            if actual.is_hard_type()
                && !actual.is_variant()
                && !actual.has_no_type()
                && !is_type_compatible_with_source(ctx, &expected_key, &actual, true, dict_id)
                && !is_type_compatible(ctx, &actual, &expected_key, false)
            {
                ctx.push_error(
                    format!(
                        r#"Cannot have a key of type "{actual}" in a dictionary of type "Dictionary[{expected_key}, {expected_value}]"."#
                    ),
                    k,
                );
                return;
            }
        }
        if let Some(v) = kv.value {
            if ctx.folds.get(v).is_some() {
                update_const_expression_builtin_type(ctx, v, &expected_value, "include");
            }
            let actual = ctx.get_type(v).clone();
            // Upstream's forward check passes `p_dictionary` (analyzer.cpp:2833).
            if actual.is_hard_type()
                && !actual.is_variant()
                && !actual.has_no_type()
                && !is_type_compatible_with_source(ctx, &expected_value, &actual, true, dict_id)
                && !is_type_compatible(ctx, &actual, &expected_value, false)
            {
                ctx.push_error(
                    format!(
                        r#"Cannot have a value of type "{actual}" in a dictionary of type "Dictionary[{expected_key}, {expected_value}]"."#
                    ),
                    v,
                );
                return;
            }
        }
    }

    // analyzer.cpp:2846-2848 — stamp the dict literal with the typed pair.
    let mut dt = ctx.get_type(dict_id).clone();
    while dt.container_element_types.len() < 2 {
        dt.container_element_types.push(DataType::variant());
    }
    dt.container_element_types[0] = expected_key;
    dt.container_element_types[1] = expected_value;
    ctx.set_type(dict_id, dt);
}

// ===================================================================================================
// reduce_call — analyzer.cpp:3231
// ===================================================================================================

/// `GDScriptAnalyzer::reduce_call(p_call, p_is_await, p_is_root)` (analyzer.cpp:3231). Godot's
/// 530-line driver covers: builtin constructors with full overload + compile-time evaluation,
/// GDScript & Variant utility-function dispatch with arg-type validation, method resolution via
/// `get_function_signature` (walking native + script + in-file class hierarchies), the
/// virtual/abstract checks for `super(...)`, the typed-array/dict literal element-type narrowing
/// against parameter types, the static-call / non-static-call cross-checks, and the
/// coroutine-must-be-awaited gate.
///
/// E3f ports the **structural skeleton** + the high-leverage cases the corpus exercises with the
/// trimmed dump:
///
/// * **Builtin constructor** (`int(x)`, `Vector3(...)`, `String(x)`, ...) — set return type to the
///   instance of the builtin (no overload validation yet; arg-type errors are a later slice).
/// * **Utility / GD-utility function** (`print`, `len`, `floor`, ...) via NativeDb. Return type
///   from the function signature. The `Cannot get return value ... void` error fires when the
///   utility returns `void` and the call isn't at statement-root / await position.
/// * **Subscript callee** (`x.method()` and `MyClass.method()`):
///   - **Native instance method**: lookup in NativeDb (walking inherits); return type from method.
///   - **In-file class member function**: lookup via `lookup_class_member`; return type from the
///     function's resolved signature.
///   - **Constructor** (`X.new()`): synthesize the instance type — Native → Native instance,
///     Class → Class instance, Script → Script instance. Godot's `is_abstract` /
///     `engine_singleton` checks at analyzer.cpp:3599-3608 fire here.
/// * **Identifier callee with non-callable resolution** (`CONSTANT(123)` where CONSTANT is an
///   `int`): emit `Member "X" is not a function.` + `Name "X" called as a function but is a "Y".`
///   (analyzer.cpp:3720-3723).
/// * **Function-not-found** for hard-typed bases (analyzer.cpp:3742-3747):
///   `Static function "X()" not found in base "Y".` on meta bases; `Function "X()" not found in
///   base Y.` on instance bases. Soft / Variant bases stay silent (mark_node_unsafe path is a
///   WP-F warning).
///
/// Full overload signature matching, the constant-fold path for compile-time constructors, the
/// `Variant::construct` invocation, super-call virtual-abstract checks, and the typed-collection
/// narrowing belong to later slices alongside `get_function_signature`'s complete port.
fn reduce_call(ctx: &mut AnalysisContext, id: NodeId, is_root: bool) {
    let call = match ctx.node(id).kind.clone() {
        NodeKind::Call(c) => c,
        _ => return,
    };

    // analyzer.cpp:3232-3243 — reduce every argument and track array/dictionary literals by index
    // so we can later push the parameter's container-element type back into them
    // (`update_array_literal_element_type` / `update_dictionary_literal_element_type`).
    let mut arrays: Vec<(usize, NodeId)> = Vec::new();
    let mut dictionaries: Vec<(usize, NodeId)> = Vec::new();
    for (i, arg) in call.arguments.iter().enumerate() {
        reduce_expression(ctx, *arg, false);
        match &ctx.node(*arg).kind {
            NodeKind::Array(_) => arrays.push((i, *arg)),
            NodeKind::Dictionary(_) => dictionaries.push((i, *arg)),
            _ => {}
        }
    }

    let mut call_type = DataType::default();

    // --- Identifier-callee branch (analyzer.cpp:3248-3533) ---------------------------------------
    let callee_kind = call
        .callee
        .map(|c| matches!(&ctx.node(c).kind, NodeKind::Identifier(_)))
        .unwrap_or(false);
    let is_subscript_callee = call
        .callee
        .map(|c| matches!(&ctx.node(c).kind, NodeKind::Subscript(_)))
        .unwrap_or(false);

    if !call.is_super && callee_kind {
        let function_name = call.function_name.clone();

        // analyzer.cpp:3252 — `Object` is treated as an error, not a constructor.
        if function_name == "Object" {
            ctx.push_error(
                r#"Invalid constructor "Object()", use "Object.new()" instead."#,
                id,
            );
            ctx.set_type(id, call_type);
            return;
        }

        // analyzer.cpp:3258 — builtin constructor.
        if let Some(bt) = crate::resolver::builtin_type_from_name(&function_name) {
            call_type = DataType {
                type_source: TypeSource::AnnotatedExplicit,
                kind: DtKind::Builtin,
                builtin_type: bt,
                ..Default::default()
            };
            // analyzer.cpp's builtin constructor UNSAFE_CALL_ARGUMENT: when a single argument
            // is provided with a soft `Variant` type and the constructor's first-positional
            // parameter set isn't `Variant`-typed, warn. Godot iterates the constructor's
            // overloads and emits the union of acceptable subtypes in the message
            // (`"Vector2" or "Vector2i"`, `"int", "bool", or "float"`, etc.). gdls has a
            // narrow per-builtin table for the four corpus shapes
            // (`warnings/unsafe_call_argument.gd`'s constructor calls).
            if !call.arguments.is_empty() {
                let arg_id = call.arguments[0];
                let at = ctx.get_type(arg_id).clone();
                // `arg.kind == Variant` covers both soft (gradual-typing fallback) and hard
                // (`var x: Variant = ...`) Variant — both yield Godot's "Variant"
                // supertype in the warning template.
                if at.kind == DtKind::Variant {
                    let subtypes: Option<(&str, &str)> = match bt {
                        VariantType::Callable => Some(("Object", "Variant")),
                        VariantType::Dictionary => Some(("Dictionary", "Variant")),
                        VariantType::Vector2 => Some((r#"Vector2" or "Vector2i"#, "Variant")),
                        VariantType::Int => Some((r#"int", "bool", or "float"#, "Variant")),
                        _ => None,
                    };
                    if let Some((sub, sup)) = subtypes {
                        let supertype = if at.is_hard_type() { "Variant" } else { sup };
                        ctx.push_warning(
                            crate::warnings::WarningCode::UnsafeCallArgument,
                            &[
                                "1".to_owned(),
                                "constructor".to_owned(),
                                function_name.clone(),
                                sub.to_owned(),
                                supertype.to_owned(),
                            ],
                            id,
                        );
                    }
                }
            }
            // Godot evaluates constructor calls over constant args into a reduced value
            // (analyzer.cpp:3327-3357); gdls can't materialize non-scalar values — an Opaque
            // fold keeps the constancy, so `match v:\n\tVector2(-1, -1):` stays a legal
            // constant pattern and `const X = Color(1, 1, 1)` stays a constant initializer.
            if call.arguments.iter().all(|&a| ctx.folds.is_reduced(a)) {
                ctx.folds.set(id, FoldedValue::Opaque(bt));
            }
            ctx.set_type(id, call_type);
            return;
        }

        // analyzer.cpp:3481 — Variant utility function (`abs`, `print`, ...).
        // analyzer.cpp:3517 — GDScript utility function (`print_debug`, `len`, `range`, ...).
        // Godot checks both `Variant::has_utility_function` and
        // `GDScriptUtilityFunctions::function_exists`; gdls checks the NativeDb first (Variant
        // utilities from extension_api.json) then a hard-coded GDScript utility table mirroring
        // `gdscript_utility_functions.cpp:570-592`.
        let utility_return = ctx
            .native
            .utility(&function_name)
            .map(|u| type_from_type_ref(ctx, &u.return_type))
            .or_else(|| gd_utility_return_type(&function_name));
        if let Some(return_type) = utility_return {
            if !is_root
                && return_type.kind == DtKind::Builtin
                && return_type.builtin_type == VariantType::Nil
            {
                ctx.push_error(
                    format!(
                        r#"Cannot get return value of call to "{function_name}()" because it returns "void"."#
                    ),
                    id,
                );
            }
            // analyzer.cpp's per-utility argument validation. Godot's
            // `gdscript_utility_functions.cpp` registers per-function `validate_arg` callbacks;
            // for `len()` the callback rejects non-length-bearing types
            // (modules/gdscript/gdscript_utility_functions.cpp's `len_func`). Mirror this
            // narrowly: when `len` is called with a single argument whose hard type isn't
            // String / StringName / Array / Dictionary / PackedXxxArray, emit Godot's
            // verbatim "Value of type 'X' can't provide a length." template.
            if function_name == "len" && call.arguments.len() == 1 {
                let arg_id = call.arguments[0];
                let at = ctx.get_type(arg_id).clone();
                if at.is_hard_type() && at.kind == DtKind::Builtin {
                    let len_ok = matches!(
                        at.builtin_type,
                        VariantType::String
                            | VariantType::StringName
                            | VariantType::Array
                            | VariantType::Dictionary
                            | VariantType::PackedByteArray
                            | VariantType::PackedInt32Array
                            | VariantType::PackedInt64Array
                            | VariantType::PackedFloat32Array
                            | VariantType::PackedFloat64Array
                            | VariantType::PackedStringArray
                            | VariantType::PackedVector2Array
                            | VariantType::PackedVector3Array
                            | VariantType::PackedColorArray
                    );
                    if !len_ok {
                        ctx.push_error(
                            format!(
                                r#"Invalid argument for "len()" function: Value of type '{at}' can't provide a length."#
                            ),
                            id,
                        );
                    }
                }
            }
            // `floor()` / `ceil()` / `round()`'s Variant utility validates Argument "x" against
            // the int/float/VectorN family (modules/variant/variant_utility.cpp). Rejecting
            // anything else with Godot's verbatim message including the `or` separator.
            if matches!(function_name.as_str(), "floor" | "ceil" | "round")
                && call.arguments.len() == 1
            {
                let arg_id = call.arguments[0];
                let at = ctx.get_type(arg_id).clone();
                if at.is_hard_type() && at.kind == DtKind::Builtin {
                    let math_ok = matches!(
                        at.builtin_type,
                        VariantType::Int
                            | VariantType::Float
                            | VariantType::Vector2
                            | VariantType::Vector2i
                            | VariantType::Vector3
                            | VariantType::Vector3i
                            | VariantType::Vector4
                            | VariantType::Vector4i
                    );
                    if !math_ok {
                        ctx.push_error(
                            format!(
                                r#"Invalid argument for "{function_name}()" function: Argument "x" must be "int", "float", "Vector2", "Vector2i", "Vector3", "Vector3i", "Vector4", or "Vector4i"."#
                            ),
                            id,
                        );
                    }
                }
            }
            ctx.set_type(id, return_type);
            return;
        }

        // No recording here: bare calls flow on into the common method-resolution below and
        // record at the SAME site as dotted/super calls, deriving the callee target from the
        // resolution the dispatch actually used (see the gate ahead of `ctx.set_type`). The
        // three early-returns above (Object, builtin constructor, native/GDScript utility)
        // bail before either point — exactly the calls with no project/native method to record.
    }

    // --- Method-call setup (analyzer.cpp:3535-3590) ----------------------------------------------
    let mut base_type = DataType::default();
    let mut is_self = false;

    if call.is_super {
        // Super-call: base = parent class. Look up the current class's base_type from ctx.bases.
        if let Some(cc) = ctx.current_class {
            let mut bt = ctx.bases.get(&cc).cloned().unwrap_or_default();
            bt.is_meta_type = false;
            base_type = bt;
            is_self = true;
        }
    } else if callee_kind {
        // Identifier callee (`foo()` from within a method): base = current class.
        if let Some(cc) = ctx.current_class {
            let mut bt = ctx.get_type(cc).clone();
            bt.is_meta_type = false;
            base_type = bt;
            is_self = true;
        }
    } else if is_subscript_callee {
        // Attribute callee (`x.method()`): the dispatcher already reduced the subscript's base
        // (analyzer.cpp:3580). Pull the base's type, with a builtin-meta short-circuit for
        // `int.foo()` / `String.bar()` etc. (analyzer.cpp:3577).
        if let Some(callee) = call.callee {
            if let NodeKind::Subscript(s) = &ctx.node(callee).kind.clone() {
                let base_id = s.base;
                if let Some(bid) = base_id {
                    if let NodeKind::Identifier(idn) = &ctx.node(bid).kind {
                        if let Some(bt) = crate::resolver::builtin_type_from_name(&idn.name) {
                            base_type = DataType {
                                type_source: TypeSource::AnnotatedExplicit,
                                kind: DtKind::Builtin,
                                builtin_type: bt,
                                is_meta_type: true,
                                is_constant: true,
                                ..Default::default()
                            };
                        }
                    }
                    if base_type.kind == DtKind::Unresolved {
                        base_type = ctx.get_type(bid).clone();
                        is_self = matches!(&ctx.node(bid).kind, NodeKind::SelfExpr);
                    }
                }
            }
        }
    } else {
        // analyzer.cpp:3584 — invalid call. Set Variant, return.
        ctx.set_type(id, DataType::variant());
        return;
    }

    // analyzer.cpp:3597 — `X.new` is a constructor when the base is a meta-type (or the callee is
    // a bare identifier — the `current_class.new()` case).
    let is_constructor = (base_type.is_meta_type || callee_kind) && call.function_name == "new";

    // analyzer.cpp:3600-3604 — engine singleton check.
    if is_constructor
        && !base_type.native_type.is_empty()
        && ctx.native.singleton_type(&base_type.native_type).is_some()
    {
        ctx.push_error(
            format!(
                r#"Cannot construct native class "{}" because it is an engine singleton."#,
                base_type.native_type
            ),
            id,
        );
        ctx.set_type(id, call_type);
        return;
    }

    // analyzer.cpp:3605-3607 — abstract class check. Godot emits
    // `Cannot construct abstract class "<Name>".` when calling `.new()` on a class declared
    // `@abstract`. For in-file `Class` bases gdls reads the flag straight off the `ClassNode`
    // (the parser sets `is_abstract = true` when an `@abstract` annotation is consumed by
    // `parse_class`); cross-file `Script` bases need the depended class's `is_abstract`
    // exposed through `CrossFileQuery`, which joins WP-N5's resolved-interface cache.
    if is_constructor && base_type.kind == DtKind::Class {
        if let Some(class_node) = base_type.class_node {
            let is_abstract = ctx.abstract_nodes.contains(&class_node)
                || matches!(
                    &ctx.node(class_node).kind,
                    NodeKind::Class(c) if c.is_abstract
                );
            if is_abstract {
                let name = match &ctx.node(class_node).kind {
                    NodeKind::Class(c) => c
                        .identifier
                        .and_then(|iid| match &ctx.node(iid).kind {
                            NodeKind::Identifier(i) => Some(i.name.clone()),
                            _ => None,
                        })
                        .unwrap_or_default(),
                    _ => String::new(),
                };
                ctx.push_error(format!(r#"Cannot construct abstract class "{name}"."#), id);
                ctx.set_type(id, call_type);
                return;
            }
        }
    }

    // Cross-file Script abstract check — same template as the in-file Class arm but uses the
    // cross-file interface (`is_file_abstract`) and the callee identifier's name for the
    // diagnostic (Godot's `script_class->fqcn` rendering reads the constant's identifier
    // name in this context, matching the corpus's
    // `const AbstractScript = preload(...)` → `Cannot construct abstract class
    // "AbstractScript".` shape).
    if is_constructor && base_type.kind == DtKind::Script {
        if let Some(script_ref) = base_type.script_type.as_ref() {
            if ctx.xfile.is_file_abstract(script_ref.file) {
                // Render the callee's base identifier as the class name (e.g.
                // `AbstractScript.new()` -> "AbstractScript"). The callee is a Subscript
                // node `<base>.new` whose `base` is the identifier; for a bare-`new()` callee
                // (no explicit base) the name is unknown — degrade to empty.
                let name = call
                    .callee
                    .and_then(|cid| match &ctx.node(cid).kind {
                        NodeKind::Subscript(s) => s.base,
                        _ => None,
                    })
                    .and_then(|bid| match &ctx.node(bid).kind {
                        NodeKind::Identifier(i) => Some(i.name.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                ctx.push_error(format!(r#"Cannot construct abstract class "{name}"."#), id);
                ctx.set_type(id, call_type);
                return;
            }
        }
    }

    // analyzer.cpp:5958-5966 — native-abstract construction. When the constructor's resolved
    // native is `!is_instantiable`, Godot emits one of three templates depending on the
    // base kind: `Class "X" cannot be constructed as it is based on abstract native class "Y".`
    // for in-file class subtypes of an abstract native, `Script "X" cannot be constructed as
    // it is based on abstract native class "Y".` for cross-file scripts (joins the
    // resolved-interface cache slice), and `Native class "Y" cannot be constructed as it is
    // abstract.` for a direct native constructor. The native_type comes off `base_type` —
    // for a meta-type call (`InstancePlaceholder.new()`) it's the class itself; for a Class
    // base it's the in-file class's native ancestor (resolved during interface walk and
    // carried on `DataType::native_type`).
    if is_constructor
        && !base_type.native_type.is_empty()
        && ctx
            .native
            .class_named(&base_type.native_type)
            .is_some_and(|c| !c.is_instantiable)
    {
        let native = base_type.native_type.clone();
        let msg = match base_type.kind {
            DtKind::Class => {
                let class_name = base_type.class_node.and_then(|c| match &ctx.node(c).kind {
                    NodeKind::Class(cls) => {
                        cls.identifier.and_then(|iid| match &ctx.node(iid).kind {
                            NodeKind::Identifier(i) => Some(i.name.clone()),
                            _ => None,
                        })
                    }
                    _ => None,
                });
                let qualified = match class_name {
                    Some(n) => format!("{}::{}", ctx.script_path, n),
                    None => ctx.script_path.to_owned(),
                };
                format!(
                    r#"Class "{qualified}" cannot be constructed as it is based on abstract native class "{native}"."#
                )
            }
            _ => format!(r#"Native class "{native}" cannot be constructed as it is abstract."#),
        };
        ctx.push_error(msg, id);
        // analyzer.cpp's super-call/abstract path falls through to identifier resolution of `new`,
        // which then emits `Name "new" is a Callable. You can call it with "new.call()" instead.`
        // as the value-called-as-function companion. The corpus pairs these two errors per call
        // site (`abstract_class_instantiate.gd`), so emit the companion alongside.
        ctx.push_error(
            r#"Name "new" is a Callable. You can call it with "new.call()" instead."#.to_owned(),
            id,
        );
        ctx.set_type(id, call_type);
        return;
    }

    // --- Method/return-type resolution -----------------------------------------------------------
    // analyzer.cpp:3610 — `get_function_signature` walks the native + script + in-file class
    // hierarchy looking for a method by name. E3f does a narrower lookup that covers the cases
    // the corpus exercises (in-file class function members; native methods via NativeDb).
    let function_name = call.function_name.clone();
    let arg_count = call.arguments.len();
    let mut return_type: Option<DataType> = None;
    let mut found = false;
    let mut sig = CallSig::default();
    let mut in_file_function_id: Option<NodeId> = None;
    // The class node DECLARING an in-file-resolved callee (the chain link
    // `lookup_class_function_or_member` found it on) — the owning class recorded on
    // `CalleeTarget::Script::class_path`.
    let mut in_file_class_id: Option<NodeId> = None;
    // WP-N1b (xref metadata only): the cross-file chain link (file + inner-class path) that
    // declares the callee, set when a call resolves through a DtKind::Script base to a Func
    // member in that script's interface. Kept separate from `in_file_function_id` so the
    // arity-error gate (~3306: `if in_file_function_id.is_some()`) remains unchanged —
    // cross-file Script calls still have no arity data, so we must not emit arity diagnostics
    // for them.
    let mut cross_file_callee: Option<crate::data_type::ScriptRef> = None;
    // The native class a successful `lookup_native_method` ran against — `CalleeTarget::Native`
    // when neither project arm resolved. Only set on a Some lookup (degrade-don't-lie: a
    // silent-Variant miss records `Unresolved`, never a guessed class).
    let mut native_callee: Option<String> = None;

    if is_constructor {
        // `X.new()` returns an instance of X (analyzer.cpp's flow ends up there via
        // `get_function_signature` finding `_init`). Synthesize directly to skip the
        // signature walk.
        let mut instance = base_type.clone();
        instance.is_meta_type = false;
        instance.is_constant = false;
        if instance.kind == DtKind::Variant {
            // Bare identifier callee on a current_class meta with no native — fall back.
            instance.kind = DtKind::Variant;
        }
        return_type = Some(instance);
        found = true;
    } else if base_type.kind == DtKind::Class {
        // In-file class method (`self.method()`, `child.method()`, `Class.method()`).
        if let Some(class_id) = base_type.class_node {
            match lookup_class_function_or_member(ctx, class_id, &function_name) {
                ClassCallLookup::Function(fn_id, declaring_class) => {
                    // analyzer.cpp:3614-3618 — super-call on abstract/virtual function.
                    if call.is_super && ctx.abstract_nodes.contains(&fn_id) {
                        ctx.push_error(
                            format!(
                                r#"Cannot call the parent class' abstract function "{function_name}()" because it hasn't been defined."#
                            ),
                            id,
                        );
                    }
                    in_file_function_id = Some(fn_id);
                    in_file_class_id = Some(declaring_class);
                    sig = function_signature(ctx, fn_id);
                    return_type = Some(sig.return_dt.clone());
                    found = true;
                }
                ClassCallLookup::NotAFunction => {
                    // analyzer.cpp:5980-5984 — `Member "X" is not a function.` fires alongside
                    // the value-callable diagnostic emitted later by reduce_identifier_from_base
                    // (the dispatcher already pre-reduced the callee, but the error still belongs
                    // on the call site so Godot's TWO-error pair lines up).
                    ctx.push_error(
                        format!(r#"Member "{function_name}" is not a function."#),
                        id,
                    );
                }
                ClassCallLookup::NotFound => {
                    // The in-file walk missed; bare inherited calls (`mirror_array(...)` with
                    // the method declared in a cross-file base) continue through the chain —
                    // analyzer.cpp's get_function_signature walks base scripts the same way.
                    if let Some(sr) = script_base_of_class(ctx, class_id) {
                        match script_chain_call(ctx, &sr, &function_name) {
                            ChainCall::Sig(chain_sig, link) => {
                                sig = *chain_sig;
                                return_type = Some(sig.return_dt.clone());
                                found = true;
                                cross_file_callee = Some(link);
                            }
                            ChainCall::Other => {}
                            ChainCall::Missing => {
                                // Interface miss: probe the chain's native root first —
                                // upstream's get_function_signature continues into ClassDB, so
                                // an inherited native method through a cross-file chain binds
                                // its real signature. Only a genuine native miss keeps the
                                // silent-Variant degrade (the interface view may be incomplete;
                                // never risk a phantom not-found).
                                let root = crate::script_chain::chain_native_root(ctx, &sr);
                                let native_sig = root.as_ref().and_then(|root| {
                                    lookup_native_method(ctx, root, &function_name)
                                });
                                if let Some(s) = native_sig {
                                    return_type = Some(s.return_dt.clone());
                                    sig = s;
                                    native_callee = root;
                                } else {
                                    return_type = Some(DataType::variant());
                                }
                                found = true;
                            }
                        }
                    } else if let Some(root) =
                        crate::resolver::nearest_native_ancestor(ctx, class_id)
                    {
                        // The chain bottoms out at a native base directly (a plain
                        // `extends Node2D` script): bind the inherited native method's
                        // signature like the explicit Native arm below. Mandatory companion to
                        // reduce_identifier_from_base's CLASS-branch native tail — without it,
                        // the not-found branch would re-reduce `self.queue_free` to a Callable
                        // VALUE and emit the bogus `Name "X" is a Callable...` error on every
                        // native method call through a class base.
                        if let Some(s) = lookup_native_method(ctx, &root, &function_name) {
                            return_type = Some(s.return_dt.clone());
                            sig = s;
                            found = true;
                            native_callee = Some(root);
                        }
                    }
                }
            }
        }
    } else if base_type.kind == DtKind::Native && !base_type.native_type.is_empty() {
        // Native method via NativeDb (walks inherits internally).
        if let Some(s) = lookup_native_method(ctx, &base_type.native_type, &function_name) {
            return_type = Some(s.return_dt.clone());
            sig = s;
            found = true;
            native_callee = Some(base_type.native_type.clone());
        }
    } else if base_type.kind == DtKind::Script {
        // WP-P1 cross-file: a Script base (`extends "f.gd"`, `extends ParentClass` where Parent
        // is class_name'd in another file, super-calls into a cross-file base). Look up the
        // method via the cross-file Interface. If the member exists as a Func, synthesize a
        // permissive CallSig from it; if it exists as a non-Func, fall through (the value-callable
        // path will fire). If it doesn't exist at all, degrade to a Variant return value
        // silently — Godot's `is_type_compatible` chain would still bind, and the "Unknown
        // stays dynamic" rule (docs/00) keeps gdls from emitting a phantom "Function not found"
        // when the cross-file interface may simply be incomplete (chain segments, inherited
        // members from a base script not yet walked, etc.).
        if let Some(script_ref) = base_type.script_type.as_ref() {
            match script_chain_call(ctx, script_ref, &function_name) {
                ChainCall::Sig(chain_sig, link) => {
                    sig = *chain_sig;
                    return_type = Some(sig.return_dt.clone());
                    found = true;
                    // WP-N1b: record the DECLARING chain link for call-edge bookkeeping —
                    // consumed at the Binding::Call recording site below (accurate references
                    // filtering).
                    cross_file_callee = Some(link);
                }
                ChainCall::Other => {
                    // Exists as a non-Func — fall through; the value-callable path fires.
                }
                ChainCall::Missing => {
                    // Not in any chain interface — probe the chain's native root first
                    // (upstream's get_function_signature continues into ClassDB), then the
                    // silent degrade ("Unknown stays dynamic"): shallow interfaces may miss
                    // inner-class methods etc., so a phantom "Function not found" is never
                    // acceptable here.
                    let root = crate::script_chain::chain_native_root(ctx, script_ref);
                    let native_sig = root
                        .as_ref()
                        .and_then(|root| lookup_native_method(ctx, root, &function_name));
                    if let Some(s) = native_sig {
                        return_type = Some(s.return_dt.clone());
                        sig = s;
                        native_callee = root;
                    } else {
                        return_type = Some(DataType::variant());
                    }
                    found = true;
                }
            }
        }
    }
    // analyzer.cpp:5937-5942 — enum base: look up Dictionary methods, reject non-const.
    // analyzer.cpp:5937-5942 — enum base: look up Dictionary methods, reject non-const.
    if !found && base_type.kind == DtKind::Enum {
        if let Some(s) = lookup_builtin_method(ctx, VariantType::Dictionary, &function_name) {
            let method_is_const = ctx
                .native
                .builtin_named("Dictionary")
                .and_then(|bt| {
                    bt.methods
                        .iter()
                        .find(|m| ctx.native.name_of(m.name) == function_name)
                })
                .is_some_and(|m| m.is_const);
            if !method_is_const {
                let enum_name = if base_type.enum_type.is_empty() {
                    base_type.native_type.clone()
                } else {
                    base_type.enum_type.clone()
                };
                ctx.push_error(
                    format!(
                        r#"Cannot call non-const Dictionary function "{function_name}()" on enum "{enum_name}"."#
                    ),
                    id,
                );
            }
            return_type = Some(s.return_dt.clone());
            sig = s;
            found = true;
        }
    }

    if !found && base_type.kind == DtKind::Builtin && base_type.builtin_type != VariantType::Nil {
        if let Some(s) = lookup_builtin_method(ctx, base_type.builtin_type, &function_name) {
            return_type = Some(s.return_dt.clone());
            sig = s;
            found = true;
        }
    }

    if found {
        // analyzer.cpp:3644-3655 — static-context call check. `is_self` (call has no explicit
        // base, or the base is `self`) + we're in a `static_context` (a static function or a
        // static-var initializer) + the resolved target is *not* static ⇒ Godot emits
        // "Cannot call non-static function X from the static function Y." (or "... from a static
        // variable initializer." if there's no enclosing function). gdls tracks the enclosing
        // function via [`AnalysisContext::current_function`]; when it's `None` we're at the
        // class-body / variable-initializer level, which is Godot's `parent_function ==
        // nullptr` branch.
        if is_self && ctx.static_context && !sig.is_static && !is_constructor {
            // analyzer.cpp:3645-3654 — name the enclosing concrete function (walking through any
            // lambda chain) so a `static_func ... var f = func (): non_static_func()` reports
            // "from the static function 'static_func()'" rather than the lambda's empty name.
            if let Some(parent_name) = enclosing_concrete_function_name(ctx) {
                ctx.push_error(
                    format!(
                        r#"Cannot call non-static function "{function_name}()" from the static function "{parent_name}()"."#
                    ),
                    id,
                );
            } else {
                ctx.push_error(
                    format!(
                        r#"Cannot call non-static function "{function_name}()" from a static variable initializer."#
                    ),
                    id,
                );
            }
        }

        // analyzer.cpp:3663 — `Cannot get return value ... void`. Godot gates on
        // `!p_is_root && !p_is_await` so an `await coroutine()` where the coroutine returns void
        // doesn't false-positive — the await IS using the value. Mirror via
        // `ctx.awaiting_call`, set in `reduce_await` for a CALL target.
        if let Some(rt) = &return_type {
            if !is_root
                && !ctx.awaiting_call
                && rt.is_hard_type()
                && rt.kind == DtKind::Builtin
                && rt.builtin_type == VariantType::Nil
            {
                ctx.push_error(
                    format!(
                        r#"Cannot get return value of call to "{function_name}()" because it returns "void"."#
                    ),
                    id,
                );
            }
        }

        // RETURN_VALUE_DISCARDED (analyzer.cpp:3684-3689, ignore-by-default): a call used as a
        // statement whose resolved return type isn't void. Builtin constructors and utility
        // functions early-return above this section and never warn — upstream's own FIXME.
        // `super._init()` is exempt, as upstream.
        if let Some(rt) = &return_type {
            if is_root
                && rt.kind != DtKind::Unresolved
                && rt.builtin_type != VariantType::Nil
                && !(call.is_super && function_name == "_init")
            {
                ctx.push_warning(
                    crate::warnings::WarningCode::ReturnValueDiscarded,
                    std::slice::from_ref(&function_name),
                    id,
                );
            }
        }

        // STATIC_CALLED_ON_INSTANCE (analyzer.cpp:3691-3694): a static method reached through
        // an instance receiver rather than the class name.
        if sig.is_static && !is_constructor && !base_type.is_meta_type && !is_self {
            let caller_type = class_identifier_name_or_default(ctx, &base_type);
            ctx.push_warning(
                crate::warnings::WarningCode::StaticCalledOnInstance,
                &[function_name.clone(), caller_type],
                id,
            );
        }

        // analyzer.cpp:3622-3636 — typed-collection arg narrowing. For each array/dictionary
        // literal whose corresponding parameter is hard-typed with container element types, push
        // the expected element type(s) into the literal (`update_array_literal_element_type` /
        // `update_dictionary_literal_element_type`), which emits the
        // `Cannot have an element/key/value of type "X" in an array/dictionary of type "..."`
        // and `Cannot include a value of type "X" as "Y".` error pair when an element is
        // incompatible. We do this for in-file functions and native methods alike; Godot
        // only requires `par_types.get(index).is_hard_type()`.
        for (i, arr_id) in &arrays {
            if let Some(par) = sig.par_types.get(*i) {
                if par.is_hard_type() && !par.container_element_types.is_empty() {
                    let elem = par.container_element_types[0].clone();
                    update_array_literal_element_type(ctx, *arr_id, &elem);
                }
            }
        }
        for (i, dict_id) in &dictionaries {
            if let Some(par) = sig.par_types.get(*i) {
                if par.is_hard_type() && par.container_element_types.len() >= 2 {
                    let key = par.container_element_types[0].clone();
                    let val = par.container_element_types[1].clone();
                    update_dictionary_literal_element_type(ctx, *dict_id, &key, &val);
                }
            }
        }

        // analyzer.cpp:6085 → validate_call_arg.
        // Count checks first (analyzer.cpp:6086-6091). For in-file functions the parameter
        // count is authoritative (we counted `parameters.len()` + default-initializer count);
        // for native methods the dump doesn't carry a per-method `default_arguments.size()`,
        // so the conservative choice is exact arity (min == max), which matches what
        // `lookup_native_method` populates.
        if in_file_function_id.is_some() {
            if arg_count < sig.min_params {
                ctx.push_error(
                    format!(
                        r#"Too few arguments for "{function_name}()" call. Expected at least {} but received {arg_count}."#,
                        sig.min_params
                    ),
                    id,
                );
            } else if !sig.is_vararg && arg_count > sig.max_params {
                ctx.push_error(
                    format!(
                        r#"Too many arguments for "{function_name}()" call. Expected at most {} but received {arg_count}."#,
                        sig.max_params
                    ),
                    id,
                );
            }
        }

        // Per-arg compatibility (analyzer.cpp:6093-6131). Godot iterates `min(args, par_types)`
        // — anything beyond is consumed by the vararg place. For each typed-hard parameter we
        // run the bidirectional `is_type_compatible` check; one direction failing turns into the
        // `Invalid argument` error. The DEBUG-only `UNSAFE_CALL_ARGUMENT` and
        // `NARROWING_CONVERSION` warnings live in WP-F.
        let n = call.arguments.len().min(sig.par_types.len());
        for i in 0..n {
            let arg_id = call.arguments[i];
            let par_type = sig.par_types[i].clone();
            // Per-arg const-narrowing (analyzer.cpp:6101-6103) — emits
            // `Cannot pass a value of type X as Y.` on a foldable constant whose type is
            // incompatible with the parameter.
            if par_type.is_hard_type() && ctx.folds.get(arg_id).is_some() {
                update_const_expression_builtin_type(ctx, arg_id, &par_type, "pass");
            }
            let arg_type = ctx.get_type(arg_id).clone();
            // Godot uses `arg_type.to_string_strict()` (gdscript_parser.h:148) which prints
            // `"Variant"` for any soft type — so `var untyped_int = 42` reads as "Variant" in the
            // warning even though its inferred kind is Builtin Int. Mirror that here.
            let arg_strict = if arg_type.is_hard_type() {
                arg_type.to_string()
            } else {
                "Variant".to_owned()
            };
            if arg_type.is_variant() || !arg_type.is_hard_type() {
                // analyzer.cpp:6109-6113 — Variant or soft argument passed to a hard non-Variant
                // parameter. Godot emits UNSAFE_CALL_ARGUMENT unless the parameter is a hard
                // Variant (then anything is fine). Symbol order:
                //  [arg_idx, "function", function_name, par_type, arg_type_strict]
                if par_type.is_hard_type() && !par_type.is_variant() {
                    ctx.push_warning(
                        crate::warnings::WarningCode::UnsafeCallArgument,
                        &[
                            format!("{}", i + 1),
                            "function".to_owned(),
                            function_name.clone(),
                            par_type.to_string(),
                            arg_strict.clone(),
                        ],
                        arg_id,
                    );
                }
                continue;
            }
            if par_type.is_hard_type()
                && !is_type_compatible(ctx, &par_type, &arg_type, true)
                && !is_type_compatible(ctx, &arg_type, &par_type, false)
            {
                ctx.push_error(
                    format!(
                        r#"Invalid argument for "{function_name}()" function: argument {} should be "{par_type}" but is "{arg_type}"."#,
                        i + 1
                    ),
                    arg_id,
                );
            } else if par_type.is_hard_type()
                && !is_type_compatible(ctx, &par_type, &arg_type, true)
                && is_type_compatible(ctx, &arg_type, &par_type, false)
            {
                // analyzer.cpp:6120-6124 — supertype is acceptable for dynamic compliance but
                // unsafe. Same UNSAFE_CALL_ARGUMENT shape as above.
                ctx.push_warning(
                    crate::warnings::WarningCode::UnsafeCallArgument,
                    &[
                        format!("{}", i + 1),
                        "function".to_owned(),
                        function_name.clone(),
                        par_type.to_string(),
                        arg_strict.clone(),
                    ],
                    arg_id,
                );
            }
        }
        call_type = return_type.unwrap_or_else(DataType::variant);
        // analyzer.cpp:6012 — `get_function_signature` stamps the resolved function's
        // `is_coroutine` flag onto the returned type so callers see it. Mirror that here so the
        // MISSING_AWAIT check below sees the right flag for in-file coroutine calls.
        if sig.is_coroutine {
            call_type.is_coroutine = true;
        }

        // analyzer.cpp:3751-3758 — MISSING_AWAIT (root) or
        // `Function "X()" is a coroutine, so it must be called with "await".` (off-root) when
        // the call returns a coroutine and isn't itself wrapped in `await`. gdls's
        // `awaiting_call` flag (set by `reduce_await` for a Call target) mirrors Godot's
        // `p_is_await` parameter.
        if call_type.is_coroutine && !ctx.awaiting_call {
            if is_root {
                ctx.push_warning(crate::warnings::WarningCode::MissingAwait, &[], id);
            } else {
                ctx.push_error(
                    format!(
                        r#"Function "{function_name}()" is a coroutine, so it must be called with "await"."#
                    ),
                    id,
                );
            }
        }
    } else {
        // --- Function-not-found branch (analyzer.cpp:3697-3748) -------------------------------
        // First try to resolve the callee identifier against the base (analyzer.cpp:3706-3729) so
        // we can emit `Name "X" called as a function but is a "Y".` when the name resolves to a
        // non-callable value.
        let callee_id = match call.callee {
            Some(cid) => match &ctx.node(cid).kind {
                NodeKind::Identifier(_) => Some(cid),
                NodeKind::Subscript(s) => match s.access {
                    Some(gd_syntax::ast::SubscriptAccess::Attribute(a)) => a,
                    _ => None,
                },
                _ => None,
            },
            None => None,
        };

        let mut name_is_value = false;
        if !call.is_super {
            if let Some(callee) = callee_id {
                reduce_identifier_from_base(ctx, callee, Some(&base_type));
                let cdt = ctx.get_type(callee).clone();
                if cdt.is_set() && !cdt.is_variant() {
                    name_is_value = true;
                    if cdt.kind == DtKind::Builtin && cdt.builtin_type == VariantType::Callable {
                        ctx.push_error(
                            format!(
                                r#"Name "{function_name}" is a Callable. You can call it with "{function_name}.call()" instead."#
                            ),
                            callee,
                        );
                    } else {
                        ctx.push_error(
                            format!(
                                r#"Name "{function_name}" called as a function but is a "{cdt}"."#
                            ),
                            callee,
                        );
                    }
                }
            }
        }

        // Emit Godot-faithful errors only when the lookup definitively failed against an
        // introspectable base. For `is_self=true` (identifier callee from within a method) with
        // no super, Godot uses `ClassDB::get_method_info` + `GDScriptUtilityFunctions` over
        // the full engine surface — gdls only sees the (possibly trimmed) NativeDb, so a missing
        // utility (`typeof`, `Color()`, …) would false-positive every such call. Permissive
        // silence on `is_self && !is_super` keeps trimmed-dump runs honest while still letting
        // the super-call + subscript-base errors fire.
        if !name_is_value
            && call.is_super
            && ctx.native.provenance() == gd_types::ApiProvenance::Exact
        {
            // v1.0.2 (issue #24): both super-miss templates below are negative claims whose
            // lookup bottoms out in the native chain — under a `Generic`/`Absent` DB a custom
            // engine build may define the member the stock surface lacks, so they only fire
            // with `Exact` provenance.
            //
            // analyzer.cpp:3742-3744 — super-call fall-through. When the function name is
            // `_init` and the parent is a native class that doesn't define a custom `_init`,
            // Godot classifies it as the virtual-constructor case and emits
            // `Cannot call the parent class' virtual function "_init()" because it hasn't been
            // defined.` (analyzer.cpp's super-call virtual-detection arm). Object's `_init` is
            // implicitly virtual on every native class, so any `super()` from `_init` against a
            // base that doesn't override it triggers this template.
            if function_name == "_init" && base_type.kind == DtKind::Native {
                ctx.push_error(
                    r#"Cannot call the parent class' virtual function "_init()" because it hasn't been defined."#.to_owned(),
                    id,
                );
            } else {
                ctx.push_error(
                    format!(
                        r#"Function "{function_name}()" not found in base {}."#,
                        base_type
                    ),
                    id,
                );
            }
        } else if !name_is_value
            && !call.is_super
            && !is_self
            && base_type.is_hard_type()
            && base_type.is_meta_type
            && base_type.kind == DtKind::Class
        {
            // analyzer.cpp:3746-3747 — static-call fall-through on a meta base
            // (e.g. `MyClass.not_existing_method()`). gdls only emits on in-file `Class` meta
            // bases — Native/Builtin/Enum metas need full ClassDB / builtin-static-method
            // introspection (Dictionary methods on enum metas, builtin static functions like
            // `Color.html_is_valid`, …) the trimmed dump can't cover. Godot's full table
            // makes those errors faithful; gdls stays permissive until the typed-collection
            // slice wires the builtin method registry.
            //
            // Godot's `DataType::to_string()` for CLASS (gdscript_parser.cpp:5339-5343)
            // returns `class_type->identifier->name` (the class's own identifier) when set,
            // else the `fqcn`. gdls's Display impl returns the placeholder `<Class>`; render
            // the identifier inline here so the corpus's `MyClass` reads through.
            let base_display = class_identifier_name_or_default(ctx, &base_type);
            ctx.push_error(
                format!(
                    r#"Static function "{function_name}()" not found in base "{base_display}"."#
                ),
                id,
            );
        }
        // Builtin instance methods (e.g. `arr.push_back()`, `dict.size()`) — Godot emits
        // "Function "X()" not found in base Y." for unknown methods on hard-typed Builtin
        // bases. gdls's trimmed dump doesn't enumerate every builtin method (Array.push_back,
        // Dictionary.has, ...), so a missed lookup is more likely a fixture gap than a real
        // user error. Degrade silently here; the typed-builtin-method registry lands with the
        // typed-collection slice.
        //
        // analyzer.cpp:3725-3727 — UNSAFE_METHOD_ACCESS warning. Godot warns when a call
        // method-lookup couldn't bind statically and the base is a soft Variant (gradual-typing
        // fallback). Gate narrowly: only when the call is a subscript-attribute access on an
        // identifier base that resolves to a function parameter declared with no annotation
        // (the canonical "untyped parameter" gradual-typing shape). This is the corpus's
        // `param.free()` case in `auto_inferred_type_dont_error.gd` and avoids the broader
        // gradual-typing surface that would false-positive every variant-base call.
        if is_subscript_callee && base_type.kind == DtKind::Variant && !base_type.is_hard_type() {
            let base_is_untyped_param = call
                .callee
                .and_then(|c| match &ctx.node(c).kind {
                    NodeKind::Subscript(s) => s.base,
                    _ => None,
                })
                .and_then(|bid| match &ctx.node(bid).kind {
                    NodeKind::Identifier(i) => Some(i.name.clone()),
                    _ => None,
                })
                .and_then(|name| {
                    ctx.current_function.and_then(|fn_id| {
                        function_param_named(ctx, fn_id, &name).and_then(|pid| {
                            match &ctx.node(pid).kind {
                                NodeKind::Parameter(p) => {
                                    // Godot's gate: parameter has no datatype_specifier
                                    // AND no infer (`:=`). Both off means an untyped default.
                                    if p.datatype_specifier.is_none() && !p.infer_datatype {
                                        Some(())
                                    } else {
                                        None
                                    }
                                }
                                _ => None,
                            }
                        })
                    })
                })
                .is_some();
            if base_is_untyped_param {
                ctx.push_warning(
                    crate::warnings::WarningCode::UnsafeMethodAccess,
                    &[function_name.clone(), "Variant".to_owned()],
                    id,
                );
            }
        }
        call_type = DataType::variant();
    }

    // Record the call site for every dispatched callee shape — bare (`helper()`), dotted
    // (`self.attack()`, `obj.method()`, `MyClass.method()`), and super (`super.method()` /
    // `super()`). Bare calls used to record at a separate pre-resolution site through a
    // file-root chain walk (WP-RD6); recording HERE instead derives the callee target from the
    // resolution the dispatch actually used — dispatch-accurate for inner-class bare calls
    // (the in-file walk starts at the CURRENT class, not the file root), and a bare name
    // shadowed by a non-Func member classifies `Unresolved` (value-callable) instead of being
    // walked past. The pre-resolution early returns (builtin constructors, utility functions,
    // `Object()`, constructor errors) never reach this point — exactly the calls with no
    // project/native method dispatch to record.
    //
    // `function_name` is the callee method name — the bare identifier, the attribute after the
    // dot for a subscript callee, or the post-`super.` identifier (resp. the enclosing
    // function for a bare `super()`), exactly as the parser fills `CallNode::function_name`.
    // Target priority:
    //   1. `cross_file_callee` — a DtKind::Script-chain hit: the declaring link's file +
    //      inner-class path.
    //   2. `in_file_function_id` — the in-file `Class` walk resolved: this file + the
    //      DECLARING class's inner path. WP-RD2: an orphan (`ctx.file` None) records
    //      `Unresolved` rather than a placeholder id.
    //   3. `native_callee` — a `lookup_native_method` hit: the class the lookup ran against
    //      (consumers resolve the DECLARING class via `NativeDb::lookup_member`).
    //   4. `Unresolved` — builtin-value/enum methods, value-callables, misses.
    // Recording is additive (WP-N1b): it sets no type and emits no diagnostic.
    if callee_kind || is_subscript_callee || call.is_super {
        let caller = caller_function_name(ctx);
        let call_span = ctx.node(id).span;
        let callee = if let Some(link) = cross_file_callee {
            CalleeTarget::Script {
                file: link.file,
                class_path: link.inner,
            }
        } else if in_file_function_id.is_some() {
            match ctx.file {
                Some(file) => CalleeTarget::Script {
                    file,
                    class_path: in_file_class_id
                        .map(|cid| class_inner_path(ctx, cid))
                        .unwrap_or_default(),
                },
                None => CalleeTarget::Unresolved,
            }
        } else if let Some(class) = native_callee {
            CalleeTarget::Native { class }
        } else {
            CalleeTarget::Unresolved
        };
        ctx.record_binding(Binding::call(
            callee,
            function_name.clone(),
            call_span,
            caller,
        ));
    }

    ctx.set_type(id, call_type);
}

// --- reduce_call helpers ---------------------------------------------------------------------------

/// A flat call-signature snapshot, just the bits `reduce_call` needs (the analyzer's per-method
/// `MethodInfo` projection at analyzer.cpp:6047-6072). `par_types` is the parameter type vector;
/// Render a `Class`-kind `DataType` the way Godot's `DataType::to_string()` does at
/// gdscript_parser.cpp:5339-5343 — the class's own identifier name when set, else the placeholder
/// `<Class>`. gdls's `Display for DataType` returns `<Class>` unconditionally because the class
/// name lives on the AST node (`ClassNode::identifier`) rather than denormalized onto `DataType`;
/// per-call-site helpers like this one read the identifier on demand so error messages render
/// `MyClass` instead of `<Class>`.
pub(crate) fn class_identifier_name_or_default(ctx: &AnalysisContext, dt: &DataType) -> String {
    if dt.kind != DtKind::Class {
        return dt.to_string();
    }
    if let Some(class_id) = dt.class_node {
        if let NodeKind::Class(c) = &ctx.node(class_id).kind {
            if let Some(iid) = c.identifier {
                if let NodeKind::Identifier(i) = &ctx.node(iid).kind {
                    if !i.name.is_empty() {
                        return i.name.clone();
                    }
                }
            }
        }
    }
    dt.to_string()
}

/// `return_dt` is the return type; the size pair is min/max arity for arg-count messages.
#[derive(Default)]
struct CallSig {
    return_dt: DataType,
    par_types: Vec<DataType>,
    min_params: usize,
    max_params: usize,
    is_vararg: bool,
    /// Whether the resolved method itself is declared `static`. Drives the static-context call
    /// check (analyzer.cpp:3644) — calling a non-static method from a static context is the
    /// "Cannot call non-static function X from the static function Y" error.
    is_static: bool,
    /// Whether the resolved function is a coroutine (its body contains `await`). Godot's
    /// parser sets `FunctionNode::is_coroutine` in `parse_await` (gdscript_parser.cpp:3232-3234)
    /// and `get_function_signature` stamps it onto the return type at analyzer.cpp:6012 so
    /// `reduce_call` can fire MISSING_AWAIT / "must be called with await".
    is_coroutine: bool,
}

/// Read an in-file function's signature snapshot for the count + per-arg compat checks. Needs
/// the function's parameters resolved (handled by interface resolution). gdls's parser doesn't
/// carry a `default_argument_count` field on `FunctionNode` (Godot's
/// `GDScriptParser::FunctionNode` at gdscript_parser.h:780), so we derive it by counting
/// `ParameterNode::initializer.is_some()`.
fn function_signature(ctx: &AnalysisContext, fn_id: NodeId) -> CallSig {
    let mut sig = CallSig::default();
    if let NodeKind::Function(f) = &ctx.node(fn_id).kind {
        sig.return_dt = ctx.get_type(fn_id).clone();
        sig.max_params = f.parameters.len();
        sig.is_vararg = f.rest_parameter.is_some();
        sig.is_static = f.is_static;
        sig.is_coroutine = f.is_coroutine;
        let defaults = f
            .parameters
            .iter()
            .filter(|p| {
                matches!(&ctx.node(**p).kind, NodeKind::Parameter(pn) if pn.initializer.is_some())
            })
            .count();
        sig.min_params = sig.max_params.saturating_sub(defaults);
        // Parameter types — resolved by `resolve_function_signature` → `resolve_parameter` →
        // `resolve_assignable`. Read directly from the type table; an unresolved param degrades
        // to whatever default ctx.get_type returns (typically Variant), which makes
        // `validate_call_arg` permissive on that slot.
        sig.par_types = f
            .parameters
            .iter()
            .map(|p| ctx.get_type(*p).clone())
            .collect();
    }
    sig
}

/// Walk in-file class + bases looking for a method `name`.
/// Outcome of looking up a callable member across the in-file class chain. Mirrors
/// Godot's `get_function_signature` walk at analyzer.cpp:5978-5988: the first member matching
/// `name` decides, and if that hit is a non-function Godot bails out with
/// `Member "X" is not a function.` rather than continuing up the chain to find a function
/// of the same name on a base. (Shadowing a base function with a non-function member is
/// rare but the find-first-wins semantics matter for the corpus's
/// `constant_used_as_function.gd` / `property_used_as_function.gd` pair.)
enum ClassCallLookup {
    /// A `func` resolved: its node + the class node DECLARING it (the chain link the walk found
    /// it on — the owning class `CalleeTarget::Script::class_path` records).
    Function(NodeId, NodeId),
    NotAFunction,
    NotFound,
}

fn lookup_class_function_or_member(
    ctx: &AnalysisContext,
    class_id: NodeId,
    name: &str,
) -> ClassCallLookup {
    let mut cur = Some(class_id);
    while let Some(class) = cur {
        if let NodeKind::Class(c) = &ctx.node(class).kind {
            if let Some(&idx) = c.members_indices.get(name) {
                return match c.members.get(idx) {
                    Some(gd_syntax::ast::Member::Function(fid)) => {
                        ClassCallLookup::Function(*fid, class)
                    }
                    Some(_) => ClassCallLookup::NotAFunction,
                    None => ClassCallLookup::NotFound,
                };
            }
        }
        let base = ctx.bases.get(&class).cloned().unwrap_or_default();
        cur = if base.kind == DtKind::Class {
            base.class_node
        } else {
            None
        };
    }
    ClassCallLookup::NotFound
}

/// Hard-coded GDScript utility function return-type table, mirroring
/// `modules/gdscript/gdscript_utility_functions.cpp:570-592`. These functions are NOT in the
/// Variant utility set (extension_api.json) — they're GDScript-only. Returns `None` for
/// unknown names so the caller falls through to the method-call dispatch.
pub(crate) fn gd_utility_return_type(name: &str) -> Option<DataType> {
    let void_dt = || DataType {
        type_source: TypeSource::AnnotatedExplicit,
        kind: DtKind::Builtin,
        builtin_type: VariantType::Nil,
        ..Default::default()
    };
    let builtin = |vt: VariantType| DataType {
        type_source: TypeSource::AnnotatedExplicit,
        kind: DtKind::Builtin,
        builtin_type: vt,
        ..Default::default()
    };
    Some(match name {
        "convert" => DataType::variant(),
        "type_exists" => builtin(VariantType::Bool),
        "char" | "_char" => builtin(VariantType::String),
        "ord" => builtin(VariantType::Int),
        "range" => builtin(VariantType::Array),
        "load" => DataType {
            type_source: TypeSource::AnnotatedExplicit,
            kind: DtKind::Native,
            builtin_type: VariantType::Object,
            native_type: "Resource".to_owned(),
            ..Default::default()
        },
        "inst_to_dict" => builtin(VariantType::Dictionary),
        "dict_to_inst" => builtin(VariantType::Object),
        "Color8" => builtin(VariantType::Color),
        "print_debug" | "print_stack" => void_dt(),
        "get_stack" => builtin(VariantType::Array),
        "len" => builtin(VariantType::Int),
        "is_instance_of" => builtin(VariantType::Bool),
        _ => return None,
    })
}

/// Look up a method on a builtin type (Array, Dictionary, String, etc.) via the NativeDb's
/// `builtin_named` table. Returns a [`CallSig`] if found, `None` otherwise.
fn lookup_builtin_method(ctx: &AnalysisContext, vt: VariantType, name: &str) -> Option<CallSig> {
    let builtin_name = data_type::variant_type_name(vt);
    let bt = ctx.native.builtin_named(builtin_name)?;
    let m = bt
        .methods
        .iter()
        .find(|m| ctx.native.name_of(m.name) == name)?;
    let return_dt = type_from_type_ref(ctx, &m.return_type);
    let par_types: Vec<DataType> = m
        .params
        .iter()
        .map(|p| type_from_type_ref(ctx, &p.ty))
        .collect();
    let n = m.params.len();
    Some(CallSig {
        return_dt,
        par_types,
        min_params: n,
        max_params: n,
        is_vararg: m.is_vararg,
        is_static: m.is_static,
        is_coroutine: false,
    })
}

/// Walk a native class's inherits chain looking for a method by name, projecting the dump's
/// `Method` record into a [`CallSig`]. The dump doesn't carry a per-method default-args count
/// (Godot's `default_arguments.size()` defaults to 0 at the `MethodInfo` level for native
/// methods until ClassDB attaches them), so we keep arity exact (`min == max`). Per-param types
/// route through [`type_from_type_ref`] — `TypeRef::Named` for which the dump has no class /
/// builtin entry degrades silently to Variant, mirroring the trimmed-dump permissiveness used
/// across the rest of reduce_call.
fn lookup_native_method(ctx: &AnalysisContext, native: &str, name: &str) -> Option<CallSig> {
    let mut cur = Some(native.to_owned());
    while let Some(c) = cur {
        let nc = ctx.native.class_named(&c)?;
        if let Some(m) = nc
            .methods
            .iter()
            .find(|m| ctx.native.name_of(m.name) == name)
        {
            let return_dt = type_from_type_ref(ctx, &m.return_type);
            let par_types: Vec<DataType> = m
                .params
                .iter()
                .map(|p| type_from_type_ref(ctx, &p.ty))
                .collect();
            let n = m.params.len();
            return Some(CallSig {
                return_dt,
                par_types,
                min_params: n,
                max_params: n,
                is_vararg: m.is_vararg,
                is_static: m.is_static,
                // Native methods never carry a coroutine flag in the dump — only in-file
                // GDScript functions can be coroutines.
                is_coroutine: false,
            });
        }
        cur = nc.inherits.map(|s| ctx.native.name_of(s).to_owned());
    }
    None
}

// ===================================================================================================
// reduce_type_test — analyzer.cpp:5175
// ===================================================================================================

/// `GDScriptAnalyzer::reduce_type_test(p_type_test)` (analyzer.cpp:5175). The expression `<x> is <T>`
/// is always a `bool`; the diagnostic interest is whether `<x>` is **provably never** of type `<T>`.
/// Two arms:
///
/// * **constant operand** (analyzer.cpp:5195-5205) — Godot uses
///   [`is_type_compatible_strict_collections`] **one-way** (`target=test_type, source=operand_type`)
///   and errors when it returns `false`. The strict-collections variant treats an untyped
///   `Array` source as incompatible with a typed `Array[T]` target (and likewise for
///   `Dictionary[K, V]`) — which is the only difference from plain `is_type_compatible`.
/// * **non-constant operand** (analyzer.cpp:5208) — bidirectional `is_type_compatible(test, operand)`
///   / `is_type_compatible(operand, test)`: both directions false **and** the operand
///   `is_hard_type` ⇒ the `Expression is of type "X" so it can't be of type "Y".` error
///   (analyzer.cpp:5210). Soft operands fall to `downgrade_node_type_source` — an `UNSAFE_CAST`-class
///   warning (WP-F); this slice stays silent on that branch.
fn reduce_type_test(ctx: &mut AnalysisContext, id: NodeId) {
    // Per analyzer.cpp:5176-5180 the test expression itself is always a `bool`.
    let mut bool_dt = DataType {
        type_source: TypeSource::AnnotatedExplicit,
        kind: DtKind::Builtin,
        builtin_type: VariantType::Bool,
        ..Default::default()
    };
    bool_dt.is_constant = false;
    let (operand, test_type_node) = match ctx.node(id).kind.clone() {
        NodeKind::TypeTest(t) => (t.operand, t.test_type),
        _ => return,
    };
    ctx.set_type(id, bool_dt);

    let Some(operand) = operand else { return };
    let Some(test_node) = test_type_node else {
        return;
    };

    reduce_expression(ctx, operand, false);
    let operand_type = ctx.get_type(operand).clone();
    let test_type = type_from_metatype(crate::resolver::resolve_datatype(ctx, Some(test_node)));

    if !operand_type.is_set() || !test_type.is_set() {
        return;
    }

    // Constant-operand arm (analyzer.cpp:5195-5205). gdls only marks a constant when the fold
    // table produced a value — the same gate as Godot's `is_constant` flag. Godot uses
    // the strict-collections compat check here so an untyped `Array` constant fails an
    // `is Array[int]` test even though the lax-collections check would pass.
    let operand_is_constant = ctx.folds.get(operand).is_some();
    if operand_is_constant {
        if !is_type_compatible_strict_collections(ctx, &test_type, &operand_type) {
            let op_str = class_identifier_name_or_default(ctx, &operand_type);
            let test_str = class_identifier_name_or_default(ctx, &test_type);
            ctx.push_error(
                format!(r#"Expression is of type "{op_str}" so it can't be of type "{test_str}"."#),
                operand,
            );
        }
        // Godot additionally folds the test to `true` via `type_from_variant` when the
        // strict-collections check passes (analyzer.cpp:5201-5203) — gdls's type_from_variant
        // doesn't yet carry the strict-collections distinction for the folded-value path, so
        // we leave the bool value unfolded. Downstream callers see Builtin Bool (non-constant)
        // which is Godot-compatible at the type level; the constant-fold is a WP-F refinement.
        return;
    }

    // Non-constant arm (analyzer.cpp:5208-5213) — bidirectional `is_type_compatible`. Both
    // directions failing on a hard-typed operand is the error path; soft operands degrade to
    // an UNSAFE_CAST warning (WP-F).
    let bidirectional_compatible = is_type_compatible(ctx, &test_type, &operand_type, false)
        || is_type_compatible(ctx, &operand_type, &test_type, false);
    if !bidirectional_compatible && operand_type.is_hard_type() {
        let op_str = class_identifier_name_or_default(ctx, &operand_type);
        let test_str = class_identifier_name_or_default(ctx, &test_type);
        ctx.push_error(
            format!(r#"Expression is of type "{op_str}" so it can't be of type "{test_str}"."#),
            operand,
        );
    }
}

// ===================================================================================================
// reduce_subscript — analyzer.cpp:4765
// ===================================================================================================

/// `GDScriptAnalyzer::reduce_subscript(p_subscript, p_can_be_pseudo_type)` (analyzer.cpp:4765).
/// Two shapes: attribute access (`base.attr`) and indexed access (`base[idx]`); E3e ports the
/// attribute path plus the constant-fold + Variant-special branches that unblock the enum-access
/// corpus family. The index path stays in the recursive-descent stub from E1 — `reduce_array` /
/// `reduce_dictionary` index-element-type propagation joins the typed-collection slice.
///
/// `can_be_pseudo_type` propagates exactly as in Godot (`true` only when this subscript is a
/// nested base of another subscript, analyzer.cpp:4772). The dispatcher always calls in with
/// `false`, so a standalone `Node.ProcessMode` triggers the
/// `Type "%s" in base "%s" cannot be used on its own.` error at analyzer.cpp:4879.
fn reduce_subscript(ctx: &mut AnalysisContext, id: NodeId, can_be_pseudo_type: bool) {
    let sub = match ctx.node(id).kind.clone() {
        NodeKind::Subscript(s) => s,
        _ => return,
    };
    let Some(base_id) = sub.base else { return };

    // Recurse into the base. analyzer.cpp:4769-4775 — identifiers and nested subscripts get the
    // pseudo-type-allowed path; everything else is a plain expression.
    match ctx.node(base_id).kind.clone() {
        NodeKind::Identifier(_) => reduce_identifier_with_flags(ctx, base_id, true),
        NodeKind::Subscript(_) => reduce_subscript(ctx, base_id, true),
        _ => reduce_expression(ctx, base_id, false),
    }

    let Some(access) = sub.access else { return };
    match access {
        gd_syntax::ast::SubscriptAccess::Attribute(attribute) => {
            reduce_subscript_attribute(ctx, id, base_id, attribute, can_be_pseudo_type);
        }
        gd_syntax::ast::SubscriptAccess::Index(index) => {
            if let Some(idx) = index {
                reduce_expression(ctx, idx, false);

                // analyzer.cpp:4916-4931 — Array / String / Packed*Array index must be int or
                // float. Godot's check has a much larger per-builtin table; this slice
                // ports the Array arm only (the targeted corpus case is `[0, 1][true]`). Other
                // base types stay permissive — they fall through to the silent-Variant tail
                // guard that the prior comment described, preserving the corpus-passing state
                // for typed-Dictionary / Vector / String-key access until the full matrix
                // lands.
                let base_type = ctx.get_type(base_id).clone();
                let index_type = ctx.get_type(idx).clone();
                if base_type.is_hard_type()
                    && base_type.kind == DtKind::Builtin
                    && base_type.builtin_type == VariantType::Array
                    && index_type.is_hard_type()
                    && index_type.kind == DtKind::Builtin
                    && index_type.builtin_type != VariantType::Int
                    && index_type.builtin_type != VariantType::Float
                {
                    ctx.push_error(
                        format!(
                            r#"Invalid index type "{index_type}" for a base of type "{base_type}"."#
                        ),
                        idx,
                    );
                }

                // Typed-Array element type: when the base is `Array[T]`, the subscript result
                // is `T` (analyzer.cpp:4933-4938 reads the container element type). This drives
                // type-test (`is String`) compatibility checks downstream
                // (`errors/constant_subscript_type.gd`'s `const base := [0]; if base[0] is
                // String:`). Untyped Array stays at the default `Variant` tail-guard. Clear
                // `is_constant` and `is_meta_type` from the element type — a subscript-load
                // always yields an instance value, never a constant or meta.
                if base_type.kind == DtKind::Builtin
                    && base_type.builtin_type == VariantType::Array
                    && !base_type.container_element_types.is_empty()
                {
                    let mut elem = base_type.container_element_types[0].clone();
                    elem.is_constant = false;
                    elem.is_meta_type = false;
                    ctx.set_type(id, elem);
                }
            }
        }
    }
}

/// The `is_attribute` branch of `reduce_subscript` (analyzer.cpp:4779-4884), split out so the index
/// branch and this branch stay legible. `attribute` is the identifier on the right of the dot.
fn reduce_subscript_attribute(
    ctx: &mut AnalysisContext,
    sub_id: NodeId,
    base_id: NodeId,
    attribute: Option<NodeId>,
    can_be_pseudo_type: bool,
) {
    let Some(attr_id) = attribute else { return };
    let base_type = ctx.get_type(base_id).clone();
    let mut result_type = DataType::default();
    let mut valid = false;

    // analyzer.cpp:4825-4842 — Variant / non-hard base. Godot's CoreConstants knows every
    // global enum; gdls only knows what the (possibly trimmed) dump exposed. When we can't find
    // a Variant.X global enum, degrade silently rather than fabricate a "Cannot find member"
    // error — the dump might just be missing it. The GDScript-constant get_named branch
    // (object-property introspection of script-typed constants) joins with the cross-file slice.
    if base_type.is_variant() || !base_type.is_hard_type() {
        valid = !base_type.is_pseudo_type || can_be_pseudo_type;
        result_type = DataType::variant();
        if base_type.is_variant()
            && base_type.is_hard_type()
            && base_type.is_meta_type
            && base_type.is_pseudo_type
        {
            // Special case: `Variant.Type` etc. — a global enum with a pseudo base.
            let enum_name = match &ctx.node(base_id).kind {
                NodeKind::Identifier(i) => {
                    let attr_name = match &ctx.node(attr_id).kind {
                        NodeKind::Identifier(a) => a.name.clone(),
                        _ => String::new(),
                    };
                    format!("{}.{}", i.name, attr_name)
                }
                _ => String::new(),
            };
            if !enum_name.is_empty() && ctx.native.global_enum(&enum_name).is_some() {
                result_type = crate::resolver::make_global_enum_type(ctx, &enum_name, "", true);
                // valid stays true.
            } else {
                // Unknown global enum (fixture-trimmed or genuinely absent). Stay permissive —
                // result_type stays Variant, valid stays whatever the pseudo gate produced.
                result_type.is_pseudo_type = false;
            }
        }
    } else {
        // analyzer.cpp:4843-4874 — concrete base. Reduce the attribute against the base's type.
        reduce_identifier_from_base(ctx, attr_id, Some(&base_type));
        let attr_type = ctx.get_type(attr_id).clone();
        if attr_type.is_set() {
            // Dictionary-with-typed-keys narrowing (analyzer.cpp:4847-4857) joins typed-collection
            // slice; skip cleanly for now (treat as the general "attribute resolved" arm).
            valid = !attr_type.is_pseudo_type || can_be_pseudo_type;
            result_type = attr_type;
            // analyzer.cpp:4861-4862 — propagate the attribute's constancy onto the subscript
            // node itself so callers (`reduce_assignment::value_is_constant`,
            // `reduce_call::validate_call_arg`'s const-arg arm) see the folded value through
            // the subscript wrapper. Without this, `EnumName.VALUE` typed correctly but
            // wasn't recognised as a constant, suppressing `update_const_expression_builtin_type`
            // and its `Cannot {assign/include/pass} a value of type X as Y` companion errors.
            if let Some(folded) = ctx.folds.get(attr_id).cloned() {
                ctx.folds.set(sub_id, folded);
            }
        } else if !base_type.is_meta_type || !base_type.is_constant {
            // analyzer.cpp:4878-4886 — the UNSAFE_PROPERTY_ACCESS arm. A property miss on a
            // non-meta or non-constant base ⇒ the lookup is dynamic, not an error; the access
            // types as Variant and Godot warns (debug builds) that the property is "not present
            // on the inferred type". The emission was deferred through v1.0.3 while the
            // attribute walk couldn't truthfully make that negative claim; the CLASS-branch
            // native tail (`await self.changed`) and its interface-gap guard closed the known
            // false positives (#32). Two deliberate deviations remain, both docs/02 §11b
            // epistemics: (1) a native-rooted miss only warns under `Exact` provenance — a
            // generic/absent DB can't disprove a custom build's member (the same gate as the
            // `!valid` arm below); (2) Script-kind bases never reach here at all — their branch
            // degrades a total miss to a silent set Variant ("an interface gap must never
            // lie"), which suppresses this warning on exactly the bases gdls sees shallowly.
            valid = base_type.kind != DtKind::Builtin;
            // The negative claim is sound only when gdls saw the base's FULL member surface:
            // a native-rooted miss needs `Exact` provenance, and a Class base whose chain root
            // is UNRESOLVABLE (e.g. extends a fork-native class under the stock embedded
            // fallback) has an incomplete surface — never warn there, under any provenance.
            let negative_is_sound = match base_type.kind {
                DtKind::Native => ctx.native.provenance() == gd_types::ApiProvenance::Exact,
                DtKind::Class => match base_type
                    .class_node
                    .and_then(|cid| crate::resolver::nearest_native_ancestor(ctx, cid))
                {
                    Some(_) => ctx.native.provenance() == gd_types::ApiProvenance::Exact,
                    None => false,
                },
                _ => true,
            };
            if valid && negative_is_sound {
                let attr_name = match &ctx.node(attr_id).kind {
                    NodeKind::Identifier(i) => i.name.clone(),
                    _ => String::new(),
                };
                // Godot passes `base_type.to_string()` RAW — no `type_from_metatype`
                // conversion, unlike the `!valid` error arm below. The helper renders
                // in-file Class kinds by identifier name and everything else via `Display`.
                let base_str = class_identifier_name_or_default(ctx, &base_type);
                ctx.push_warning(
                    crate::warnings::WarningCode::UnsafePropertyAccess,
                    &[attr_name, base_str],
                    sub_id,
                );
            }
            result_type = DataType::variant();
        }
    }

    if !valid {
        let attr_type = ctx.get_type(attr_id).clone();
        let base_instance = crate::resolver::type_from_metatype(base_type);
        // v1.0.2 (issue #24): a member miss on a base whose lookup chain reaches the native
        // surface (a Native base, or any script/class meta rooted in one — inherited native
        // constants resolve through subclass names) is only a trustworthy negative under
        // `Exact` provenance; a `Generic`/`Absent` DB can't disprove a custom engine build's
        // member. Degrade to Variant silently, like the non-meta UNSAFE path above.
        if ctx.native.provenance() != gd_types::ApiProvenance::Exact
            && (base_instance.kind == DtKind::Native || !base_instance.native_type.is_empty())
        {
            ctx.set_type(sub_id, DataType::variant());
            return;
        }
        // Render in-file `Class`-kind bases with their identifier name instead of the
        // `Display`-side `<Class>` placeholder (analyzer.cpp:5339-5343, see
        // `class_identifier_name_or_default`).
        let base_str = class_identifier_name_or_default(ctx, &base_instance);
        let attr_name = match &ctx.node(attr_id).kind {
            NodeKind::Identifier(i) => i.name.clone(),
            _ => String::new(),
        };
        if !can_be_pseudo_type && (attr_type.is_pseudo_type || result_type.is_pseudo_type) {
            ctx.push_error(
                format!(r#"Type "{attr_name}" in base "{base_str}" cannot be used on its own."#),
                attr_id,
            );
        } else {
            ctx.push_error(
                format!(r#"Cannot find member "{attr_name}" in base "{base_str}"."#),
                attr_id,
            );
        }
        result_type = DataType::variant();
    }

    ctx.set_type(sub_id, result_type);
}

/// Outcome of a cross-file call-signature walk over a script chain.
enum ChainCall {
    /// A Func member resolved: its synthesized [`CallSig`] + the DECLARING chain link (file +
    /// inner-class path — what `CalleeTarget::Script` records). Boxed — the signature dwarfs
    /// the unit variants and this enum moves through match arms by value.
    Sig(Box<CallSig>, crate::data_type::ScriptRef),
    /// The name exists as a non-Func member — the value-callable path owns it.
    Other,
    /// Not in any chain interface.
    Missing,
}

/// Look up `function_name` as a Func through `start`'s full extends chain, synthesizing a
/// permissive [`CallSig`]: the return type projects through the declared annotation (what kills
/// the `x := obj.method()` INFERENCE_ON_VARIANT false positives) while params stay Variant —
/// arity is exact (`required_params` carries defaults), argument-type errors against a shallow
/// interface are not worth the false-positive risk.
fn script_chain_call(
    ctx: &mut AnalysisContext,
    start: &crate::data_type::ScriptRef,
    function_name: &str,
) -> ChainCall {
    let chain = crate::script_chain::resolve_script_chain(ctx, start);
    let xf = ctx.xfile;
    let mut hit: Option<(crate::data_type::ScriptRef, &gd_project::MemberDecl)> = None;
    for link in chain.links.iter() {
        let Some(iface) = crate::script_chain::link_interface(xf, link) else {
            continue;
        };
        if let Some(m) = iface.members.iter().find(|m| m.name == function_name) {
            if m.kind != gd_project::MemberKind::Func {
                return ChainCall::Other;
            }
            hit = Some((link.clone(), m));
            break;
        }
    }
    let Some((link, member)) = hit else {
        return ChainCall::Missing;
    };
    let par_n = member.params.len();
    let return_dt = resolve_interface_type_expr(ctx, link.file, &member.ty);
    ChainCall::Sig(
        Box::new(CallSig {
            return_dt,
            par_types: vec![DataType::variant(); par_n],
            min_params: member.required_params,
            max_params: par_n,
            is_vararg: false,
            is_static: member.flags.is_static,
            is_coroutine: member.flags.is_coroutine,
        }),
        link,
    )
}

// ===================================================================================================
// resolve_interface_type_expr — the cross-file analog of resolve_datatype (analyzer.cpp:654-900)
// ===================================================================================================

/// Project a cross-file `MemberDecl`'s syntactic [`gd_project::TypeExpr`] into the lattice. The
/// names were written in the DECLARING file's scope, so they resolve through interfaces only —
/// mirroring `resolve_datatype`'s order (Variant → builtin → native → global class → file scope →
/// global enum) minus the pieces an interface can't see (locals, outer-class consts).
///
/// NEVER pushes a diagnostic and NEVER returns Unresolved: an unresolvable name degrades to
/// Variant ("unknown stays dynamic", docs/00). `TypeExpr::None` is also Variant — deliberately
/// NOT void, because interface extraction collapses `-> void` and "no annotation" into `None`,
/// and a hard void would false-positive `Cannot get return value of call to "X()" because it
/// returns "void".` on every unannotated cross-file function.
pub(crate) fn resolve_interface_type_expr(
    ctx: &mut AnalysisContext,
    declaring_file: gd_project::FileId,
    ty: &gd_project::TypeExpr,
) -> DataType {
    let gd_project::TypeExpr::Named { path, args } = ty else {
        return DataType::variant();
    };
    let Some(first) = path.first().map(String::as_str) else {
        return DataType::variant();
    };

    let mut result = DataType {
        type_source: TypeSource::AnnotatedExplicit,
        ..Default::default()
    };

    if first == "Variant" {
        result.kind = DtKind::Variant;
    } else if let Some(builtin) = crate::resolver::builtin_type_from_name(first) {
        // Two segments under a builtin head: an interface-captured `Color.PURPLE`-style
        // initializer (the constant's DECLARED type from the dump — `Vector3.AXIS_X` is int)
        // or a builtin enum (`Vector3.Axis`).
        if path.len() == 2 {
            let builtin_name = crate::data_type::variant_type_name(builtin);
            if let Some(bt) = ctx.native.builtin_named(builtin_name) {
                if let Some(c) = bt
                    .constants
                    .iter()
                    .find(|c| ctx.native.name_of(c.name) == path[1])
                {
                    let const_bt =
                        c.ty.and_then(|sym| {
                            crate::resolver::builtin_type_from_name(ctx.native.name_of(sym))
                        })
                        .unwrap_or(builtin);
                    result.kind = DtKind::Builtin;
                    result.builtin_type = const_bt;
                    return result;
                }
                if bt
                    .enums
                    .iter()
                    .any(|e| ctx.native.name_of(e.name) == path[1])
                {
                    return crate::resolver::make_builtin_enum_type(ctx, &path[1], builtin, false);
                }
            }
            return DataType::variant();
        }
        result.kind = DtKind::Builtin;
        result.builtin_type = builtin;
        // `Array[T]` / `Dictionary[K, V]` element types recurse through the same resolver
        // (analyzer.cpp:894-925's container walk, interface-shaped).
        let expected = match builtin {
            VariantType::Array => 1,
            VariantType::Dictionary => 2,
            _ => 0,
        };
        if expected > 0 && !args.is_empty() {
            for arg in args.iter().take(expected) {
                result
                    .container_element_types
                    .push(resolve_interface_type_expr(ctx, declaring_file, arg));
            }
            while result.container_element_types.len() < expected {
                result.container_element_types.push(DataType::variant());
            }
        }
    } else if ctx.native.class_named(first).is_some() {
        if path.len() == 2 && native_has_enum(ctx, first, &path[1]) {
            // `TileSet.TileShape` — a native-class enum; a member of that type holds a VALUE.
            result = crate::resolver::make_native_enum_type(ctx, &path[1], first, false);
        } else {
            result.kind = DtKind::Native;
            result.builtin_type = VariantType::Object;
            result.native_type = first.to_owned();
        }
    } else if let Some(fid) = ctx.xfile.global_class_file(first) {
        // Project `class_name` → Script INSTANCE. `Outer.SomeEnum` as the trailing segment is
        // an enum of that file; deeper segments walk its inner classes. Misses degrade.
        if path.len() == 2 {
            if let Some(dt) = cross_file_enum_instance(ctx, fid, &path[1]) {
                return dt;
            }
        }
        if path.len() > 1 {
            let chain: Vec<&str> = path[1..].iter().map(String::as_str).collect();
            if ctx.xfile.resolve_inner_chain(fid, &chain).is_some() {
                return script_instance_datatype(ctx, fid, path[1..].to_vec());
            }
            return DataType::variant();
        }
        return script_instance_datatype(ctx, fid, Vec::new());
    } else if let Some(dt) = cross_file_enum_instance(ctx, declaring_file, first) {
        // A named enum of the declaring file itself (`var mode: Mode`).
        return dt;
    } else if path.len() == 1
        && ctx
            .xfile
            .resolve_inner_chain(declaring_file, &[first])
            .is_some()
    {
        // An inner class of the declaring file (`var helper: InnerHelper`).
        return script_instance_datatype(ctx, declaring_file, path.clone());
    } else if ctx.native.global_enum(first).is_some() {
        result = crate::resolver::make_global_enum_type(ctx, first, "", false);
    } else {
        return DataType::variant();
    }

    result
}

/// A Script-typed INSTANCE DataType for `file` (+ inner-class chain). The cross-file counterpart
/// of `resolver::script_base_datatype` with `is_meta_type = false`.
pub(crate) fn script_instance_datatype(
    ctx: &AnalysisContext,
    file: gd_project::FileId,
    inner: Vec<String>,
) -> DataType {
    let sref = crate::data_type::ScriptRef { file, inner };
    DataType {
        kind: DtKind::Script,
        type_source: TypeSource::AnnotatedExplicit,
        is_meta_type: false,
        builtin_type: VariantType::Object,
        native_type: crate::script_chain::chain_native_root(ctx, &sref).unwrap_or_default(),
        script_type: Some(sref),
        ..Default::default()
    }
}

/// Look up `name` through `start`'s full extends chain (`crate::script_chain`), typing each hit
/// per Godot's member-kind conditions in the `script_classes` walk (analyzer.cpp:4188-4260):
/// named enums and constants always; variables if `!base_is_meta || static`; signals if
/// `!base_is_meta`; functions if `!base_is_meta || static`. A name that matches but fails its
/// condition keeps walking (Godot's switch falls through to the next class). On a hit, records a
/// [`Binding::Use`] against the DECLARING link's file — what `textDocument/references` and
/// `definition` project for member-access sites — and returns the type (+ optional fold).
fn lookup_script_chain_member(
    ctx: &mut AnalysisContext,
    start: &crate::data_type::ScriptRef,
    name: &str,
    base_is_meta: bool,
    bind_site: NodeId,
) -> Option<(DataType, Option<FoldedValue>)> {
    let chain = crate::script_chain::resolve_script_chain(ctx, start);
    let xf = ctx.xfile;
    for link in chain.links.iter() {
        let Some(iface) = crate::script_chain::link_interface(xf, link) else {
            continue;
        };
        // Named enum — Godot's ENUM member arm (no access-mode condition; reachable through
        // meta AND instance bases). Yields the enum's META type; `.VALUE` then lands in the
        // Enum-meta arm above.
        if let Some(enum_decl) = iface.enums.iter().find(|e| e.name == name) {
            let base_path = link_basename(ctx, link);
            let mut dt = crate::resolver::make_enum_type(name, &base_path, true);
            dt.script_type = Some(link.clone());
            dt.is_constant = true;
            for (i, v) in enum_decl.values.iter().enumerate() {
                match v.value {
                    Some(val) => {
                        dt.enum_values.insert(v.name.clone(), val);
                    }
                    None => {
                        dt.enum_values_inexact = true;
                        dt.enum_values.insert(v.name.clone(), i as i64);
                    }
                }
            }
            record_member_use(ctx, link, BindingSymbolKind::Enum, name, bind_site);
            return Some((dt, None));
        }
        let Some(member) = iface.members.iter().find(|m| m.name == name) else {
            continue;
        };
        use gd_project::MemberKind as MK;
        match member.kind {
            MK::Const => {
                if iface.unnamed_enum_values.iter().any(|v| v == name) {
                    // Anonymous-enum hoist — Godot's ENUM_VALUE arm (analyzer.cpp:4203-4209).
                    let base_path = link_basename(ctx, link);
                    let mut dt =
                        crate::resolver::make_enum_type("<anonymous enum>", &base_path, false);
                    dt.script_type = Some(link.clone());
                    dt.is_constant = true;
                    record_member_use(ctx, link, BindingSymbolKind::EnumValue, name, bind_site);
                    // Placeholder fold: `is_reduced` gates read only the type.
                    return Some((dt, Some(FoldedValue::Int(0))));
                }
                // CONSTANT arm (analyzer.cpp:4193-4200): the member's declared type; untyped
                // consts degrade to soft Variant — permissive, never an enum value.
                let mut dt = resolve_interface_type_expr(ctx, link.file, &member.ty);
                dt.is_constant = true;
                record_member_use(ctx, link, BindingSymbolKind::Constant, name, bind_site);
                // No fold: the value isn't materializable cross-file; every fold consumer is an
                // `is_reduced` gate or type-only narrowing, so absence only skips
                // const-companion diagnostics — it never adds one.
                return Some((dt, None));
            }
            MK::Var | MK::Property => {
                if base_is_meta && !member.flags.is_static {
                    continue; // VARIABLE arm condition (analyzer.cpp:4219-4226)
                }
                let mut dt = resolve_interface_type_expr(ctx, link.file, &member.ty);
                // A VAR member is never a constant, whatever its TYPE carries — enum-typed
                // members (`var key: Key`) get instance types whose constructors mark
                // is_constant, and leaving it set made `obj.key = x` error
                // `Cannot assign a new value to a constant.` project-wide.
                dt.is_constant = false;
                dt.is_read_only = false;
                record_member_use(ctx, link, BindingSymbolKind::Variable, name, bind_site);
                return Some((dt, None));
            }
            MK::Signal => {
                if base_is_meta {
                    continue; // SIGNAL arm condition (analyzer.cpp:4229-4236)
                }
                let params = member
                    .params
                    .iter()
                    .zip(
                        member
                            .param_names
                            .iter()
                            .map(String::clone)
                            .chain(std::iter::repeat(String::new())),
                    )
                    .map(|(ty, pname)| (pname, resolve_interface_type_expr(ctx, link.file, ty)))
                    .collect();
                let dt = crate::resolver::make_signal_type(crate::data_type::MethodSig {
                    name: name.to_owned(),
                    params,
                    return_type: Box::new(DataType::default()),
                });
                record_member_use(ctx, link, BindingSymbolKind::Signal, name, bind_site);
                return Some((dt, None));
            }
            MK::Func => {
                if base_is_meta && !member.flags.is_static {
                    continue; // FUNCTION arm condition (analyzer.cpp:4239-4246)
                }
                // Constant Callable — parity with the in-file Class arm; the full signature
                // lives with reduce_call's cross-file CallSig path.
                let mut dt = DataType {
                    type_source: TypeSource::AnnotatedExplicit,
                    kind: DtKind::Builtin,
                    builtin_type: VariantType::Callable,
                    is_constant: true,
                    ..Default::default()
                };
                dt.is_meta_type = false;
                record_member_use(ctx, link, BindingSymbolKind::Function, name, bind_site);
                return Some((dt, None));
            }
            // Named enums are matched through `iface.enums` above; the member-list entry is
            // only the declaration marker.
            MK::Enum => continue,
        }
    }
    None
}

/// Record the `Binding::Use` for a resolved cross-file member access — the declaring link's
/// file, never the access-site file (definition/references project straight to the declaration).
fn record_member_use(
    ctx: &mut AnalysisContext,
    link: &crate::data_type::ScriptRef,
    kind: BindingSymbolKind,
    name: &str,
    bind_site: NodeId,
) {
    let site = ctx.node(bind_site).span;
    ctx.record_binding(Binding::use_(Some(link.file), kind, name.to_owned(), site));
}

/// The basename of a chain link's file, for the `<file.gd>.<EnumName>` fqcn shape Godot's
/// `make_class_enum_type` renders for cross-file enums (analyzer.cpp:147).
fn link_basename(ctx: &AnalysisContext, link: &crate::data_type::ScriptRef) -> String {
    ctx.xfile
        .file_path(link.file)
        .map(|p| p.rsplit(['/', '\\']).next().unwrap_or(p).to_owned())
        .unwrap_or_default()
}

/// A cross-file named enum of `file`'s HEAD class, meta or instance, values populated from the
/// interface (real declared integers where syntactically known; placeholders mark the map
/// inexact). `None` when `file` has no such named enum. Shared by the reducer's Script-branch
/// enum arm, the const-annotation resolver, and `resolve_datatype`'s Script-segment walk.
pub(crate) fn cross_file_named_enum(
    ctx: &AnalysisContext,
    file: gd_project::FileId,
    name: &str,
    meta: bool,
) -> Option<DataType> {
    let enum_decl = ctx.xfile.lookup_file_enum(file, name)?;
    let base_path = ctx
        .xfile
        .file_path(file)
        .map(|p| p.rsplit(['/', '\\']).next().unwrap_or(p).to_owned())
        .unwrap_or_default();
    let mut dt = crate::resolver::make_enum_type(name, &base_path, meta);
    dt.script_type = Some(crate::data_type::ScriptRef {
        file,
        inner: Vec::new(),
    });
    dt.is_constant = true;
    for (i, v) in enum_decl.values.iter().enumerate() {
        match v.value {
            Some(val) => {
                dt.enum_values.insert(v.name.clone(), val);
            }
            None => {
                dt.enum_values_inexact = true;
                dt.enum_values.insert(v.name.clone(), i as i64);
            }
        }
    }
    Some(dt)
}

/// Instance-typed wrapper (a VALUE of the enum) — the const/annotation consumer shape.
fn cross_file_enum_instance(
    ctx: &AnalysisContext,
    file: gd_project::FileId,
    name: &str,
) -> Option<DataType> {
    cross_file_named_enum(ctx, file, name, false)
}

// ===================================================================================================
// reduce_identifier_from_base — analyzer.cpp:4024
// ===================================================================================================

/// `GDScriptAnalyzer::reduce_identifier_from_base(p_identifier, p_base)` (analyzer.cpp:4024). Look
/// up `p_identifier` as a member of `p_base`. E3e ports the enum-meta / builtin-meta / native-class
/// branches that drive the enum-access corpus family; the in-file class branch (script_classes
/// walk at analyzer.cpp:4150-4251) walks `lookup_class_member`, continues into the cross-file
/// chain, and finishes with the chain's native root (`try_native_member`, v1.0.4 #32 — upstream
/// FALLS THROUGH from the class loop to the native check at :4324, so `self.changed` on a
/// `extends Resource` class resolves the inherited native signal); the Script branch (4253-4306)
/// runs the full per-kind member walk over `crate::script_chain` (v1.0.1) with the same tail.
///
/// `base = None` is Godot's "no explicit base" path that defaults to `current_class`. gdls's
/// caller in `reduce_subscript` always passes an explicit base, so we don't yet exercise that arm.
fn reduce_identifier_from_base(
    ctx: &mut AnalysisContext,
    identifier_id: NodeId,
    base: Option<&DataType>,
) {
    // analyzer.cpp:4025-4027 — don't re-resolve.
    if !ctx.get_type(identifier_id).has_no_type() {
        return;
    }
    let raw_name = match &ctx.node(identifier_id).kind {
        NodeKind::Identifier(i) => i.name.clone(),
        _ => return,
    };
    let base = match base {
        Some(b) => b.clone(),
        None => {
            // analyzer.cpp:4030-4034 — fall back to `current_class`'s metatype.
            let Some(cc) = ctx.current_class else { return };
            crate::resolver::type_from_metatype(ctx.get_type(cc).clone())
        }
    };

    // analyzer.cpp:4158 — `Foo.new` on a meta-type rewrites to `_init` for the lookup pass below.
    let is_constructor = base.is_meta_type && raw_name == "new";
    let name = if is_constructor {
        "_init".to_owned()
    } else {
        raw_name.clone()
    };

    // --- ENUM branch (analyzer.cpp:4038-4053) ----------------------------------------------------
    if base.kind == DtKind::Enum {
        if base.is_meta_type {
            if let Some(&val) = base.enum_values.get(&name) {
                ctx.set_type(identifier_id, type_from_metatype(base));
                ctx.folds.set(identifier_id, FoldedValue::Int(val));
            }
            // analyzer.cpp:4047-4048: not found ⇒ leave datatype unset, return to the
            // `Cannot find member` arm in reduce_subscript.
            return;
        }
        ctx.push_error("Cannot get property from enum value.", identifier_id);
        return;
    }

    // --- BUILTIN branch (analyzer.cpp:4055-4147) -------------------------------------------------
    if base.kind == DtKind::Builtin {
        if base.is_meta_type {
            let builtin_name = data_type::variant_type_name(base.builtin_type);
            let Some(bt) = ctx.native.builtin_named(builtin_name) else {
                // Base isn't in the (possibly trimmed) NativeDb. Godot's `ClassDB` has every
                // builtin always, so this is gdls-specific. Per the "never crash, never lie" rule
                // we degrade to Variant rather than invent a phantom "Cannot find member" — the
                // caller still gets a determined type from the attr (so the subscript's
                // pseudo-type / Cannot-find-member checks short-circuit on a benign Variant).
                ctx.set_type(identifier_id, DataType::variant());
                return;
            };

            // 1. Constants (analyzer.cpp:4059-4067). Godot types these by `type_from_variant`
            //    over the constant's real value; the dump carries each constant's declared type,
            //    which is the same information without materializing the value (`Vector3.UP` →
            //    Vector3, `Vector3.AXIS_X` → int). Fall back to the parent builtin if the dump
            //    omitted the type (every builtin's same-typed constants make that a safe default).
            if let Some(c) = bt
                .constants
                .iter()
                .find(|c| ctx.native.name_of(c.name) == name)
            {
                let const_bt =
                    c.ty.and_then(|sym| {
                        crate::resolver::builtin_type_from_name(ctx.native.name_of(sym))
                    })
                    .unwrap_or(base.builtin_type);
                let typed = DataType {
                    type_source: TypeSource::AnnotatedExplicit,
                    kind: DtKind::Builtin,
                    builtin_type: const_bt,
                    is_constant: true,
                    ..Default::default()
                };
                ctx.set_type(identifier_id, typed);
                // Godot folds the constant's real value (`Variant::get_constant_value`,
                // analyzer.cpp:4063); `FoldedValue` has no vector/color representations, so stamp
                // an `Opaque` fold instead — downstream `is_reduced` gates still see a constant
                // expression, while value-dependent paths (binary fold, dup-key) know there is no
                // trustworthy value behind it.
                ctx.folds.set(identifier_id, FoldedValue::Opaque(const_bt));
                return;
            }

            // 2. Value belonging to an enum (analyzer.cpp:4069-4078).
            for ne in &bt.enums {
                if let Some((_, val)) = ne
                    .values
                    .iter()
                    .find(|(sym, _)| ctx.native.name_of(*sym) == name)
                {
                    let enum_name = ctx.native.name_of(ne.name).to_owned();
                    let t = crate::resolver::make_builtin_enum_type(
                        ctx,
                        &enum_name,
                        base.builtin_type,
                        false,
                    );
                    ctx.set_type(identifier_id, t);
                    ctx.folds.set(identifier_id, FoldedValue::Int(*val));
                    return;
                }
            }

            // 3. The enum itself (analyzer.cpp:4080-4084).
            if bt.enums.iter().any(|e| ctx.native.name_of(e.name) == name) {
                let t =
                    crate::resolver::make_builtin_enum_type(ctx, &name, base.builtin_type, true);
                ctx.set_type(identifier_id, t);
                return;
            }

            // 4. Not found. Godot emits "Cannot find member" via the subscript caller — gdls's
            //    subscript path emits the same. Leave datatype unset so the caller sees the
            //    definitive "not found" verdict.
            return;
        }
        // Non-meta builtin base (instance): members (`pos.x` → int, analyzer.cpp:4118-4124 via
        // Variant introspection) and methods (constant Callable). Unknown names stay a silent
        // Variant — the trimmed-DB / dynamic-member rule.
        let builtin_name = data_type::variant_type_name(base.builtin_type);
        if let Some(bt) = ctx.native.builtin_named(builtin_name) {
            if let Some(member) = bt
                .members
                .iter()
                .find(|m| ctx.native.name_of(m.name) == name)
            {
                let dt = type_from_type_ref(ctx, &member.ty);
                ctx.set_type(identifier_id, dt);
                return;
            }
            if bt
                .methods
                .iter()
                .any(|m| ctx.native.name_of(m.name) == name)
            {
                let mut t = DataType {
                    type_source: TypeSource::AnnotatedExplicit,
                    kind: DtKind::Builtin,
                    builtin_type: VariantType::Callable,
                    is_constant: true,
                    ..Default::default()
                };
                t.is_meta_type = false;
                ctx.set_type(identifier_id, t);
                return;
            }
        }
        ctx.set_type(identifier_id, DataType::variant());
        return;
    }

    // --- CLASS branch (analyzer.cpp:4150-4251) ---------------------------------------------------
    // Walk the in-file class chain via the existing `lookup_class_member` machinery; that helper
    // already handles the RESOLVING-sentinel cycle trigger through `resolve_class_member_by_name`.
    if base.kind == DtKind::Class {
        if let Some(class_id) = base.class_node {
            if let Some((member_dt, fold)) =
                lookup_class_member(ctx, class_id, &name, identifier_id)
            {
                // Record the resolved in-file attribute read (`self.hp`, an access on a base
                // typed as this file's own class) — the in-file twin of `record_member_use`'s
                // cross-file recording, closing the last attribute-read recording gap so
                // references can ride bindings instead of the raw identifier scan. WP-RD2: an
                // orphan records `None` ("don't know"), never a placeholder id. Additive
                // (WP-N1b): no type or diagnostic changes.
                let site = ctx.node(identifier_id).span;
                ctx.record_binding(Binding::use_(
                    ctx.file,
                    BindingSymbolKind::Member,
                    name.clone(),
                    site,
                ));
                ctx.set_type(identifier_id, member_dt);
                if let Some(fv) = fold {
                    ctx.folds.set(identifier_id, fv);
                }
                return;
            }
            // The in-file walk missed; continue into the cross-file part of the chain (the
            // analyzer.cpp:4166-4267 script_classes loop crossing the file boundary) — this is
            // what types `self.hp` when `hp` lives in a cross-file base.
            if let Some(sr) = script_base_of_class(ctx, class_id) {
                if let Some((dt, fold)) =
                    lookup_script_chain_member(ctx, &sr, &name, base.is_meta_type, identifier_id)
                {
                    if let Some(fv) = fold {
                        ctx.folds.set(identifier_id, fv);
                    }
                    ctx.set_type(identifier_id, dt);
                    return;
                }
            }
            // Native tail (analyzer.cpp:4324-4360) — upstream's single function FALLS THROUGH
            // from the class loop to the native-member check for every base kind; this is the
            // CLASS branch's leg of that fall-through. `await self.changed` on a
            // `extends Resource` class resolves the inherited native signal here (issue #32's
            // corpus pin, await_with_signals_no_warning.gd). A hit types the identifier; a miss
            // falls through to the caller's error/warning semantics, like upstream.
            if let Some(root) = crate::resolver::nearest_native_ancestor(ctx, class_id) {
                if try_native_member(ctx, &root, &name, is_constructor, identifier_id) {
                    return;
                }
            }
        }
        // Constructor (`.new` ⇒ `_init`) on an in-file class: synthesize a Callable so the
        // outer subscript doesn't error on legitimate `MyClass.new()` patterns. The full call
        // signature lives with `reduce_call`; until then a Callable type is sufficient to clear
        // the "Cannot find member new" path.
        if is_constructor {
            let mut t = DataType {
                type_source: TypeSource::AnnotatedExplicit,
                kind: DtKind::Builtin,
                builtin_type: VariantType::Callable,
                is_constant: true,
                ..Default::default()
            };
            t.is_meta_type = false;
            ctx.set_type(identifier_id, t);
            return;
        }
        // Not found anywhere. When the chain crossed a file boundary the interface view may be
        // incomplete — degrade the non-meta/non-constant case to a SILENT Variant (the SCRIPT
        // branch's never-lie rule below) instead of leaving the type unset, which would read as
        // a trustworthy miss to the caller's UNSAFE_PROPERTY_ACCESS arm. The meta+constant case
        // stays unset so `Cannot find member` (and its provenance gate) behaves exactly as
        // before.
        if base.class_node.is_some_and(|cid| {
            script_base_of_class(ctx, cid).is_some() && (!base.is_meta_type || !base.is_constant)
        }) {
            ctx.set_type(identifier_id, DataType::variant());
            return;
        }
        // Not found — leave datatype unset, let the caller (reduce_subscript) emit
        // "Cannot find member" at the reference site.
        return;
    }

    // --- SCRIPT branch (analyzer.cpp:4253-4306) ---------------------------------------------------
    // Cross-file member resolution over the full extends chain (v1.0.1): named enums, consts,
    // vars, signals, and functions type per Godot's access-mode conditions, then the chain's
    // native root carries the inherited native members. The one deliberate deviation: a complete
    // miss still degrades to a SILENT Variant rather than `Cannot find member` — interfaces are
    // shallow extracts and a gap in them must never become an error.
    if base.kind == DtKind::Script {
        if is_constructor {
            let mut t = DataType {
                type_source: TypeSource::AnnotatedExplicit,
                kind: DtKind::Builtin,
                builtin_type: VariantType::Callable,
                is_constant: true,
                ..Default::default()
            };
            t.is_meta_type = false;
            ctx.set_type(identifier_id, t);
            return;
        }
        // The script_classes member walk (analyzer.cpp:4188-4260) over the FULL extends chain:
        // named enums, consts (incl. anonymous-enum hoists), vars, signals, and functions, each
        // typed per Godot's access-mode conditions, with a `Binding::Use` recorded against the
        // declaring file. Misses continue into cycle detection, then the chain's native root.
        if let Some(sr) = base.script_type.clone() {
            if let Some((dt, fold)) =
                lookup_script_chain_member(ctx, &sr, &name, base.is_meta_type, identifier_id)
            {
                if let Some(fv) = fold {
                    ctx.folds.set(identifier_id, fv);
                }
                ctx.set_type(identifier_id, dt);
                return;
            }
        }
        if base.is_meta_type {
            if let Some(sr) = base.script_type.as_ref() {
                // WP-R2: cross-file mutual member cycle detection. Godot drives this via
                // `resolve_class_member`'s recursive external path (analyzer.cpp:1001-1024):
                // when A's `var v = A.v` reduces `A.v`, A's analyzer calls into B's analyzer
                // for B's `v` resolution; B in turn calls back into A's analyzer for `A.v`,
                // hits the per-member `DataType::RESOLVING` guard (analyzer.cpp:984-991),
                // and pushes `Could not resolve member "v": Cyclic reference.` onto B's
                // parser. A's outer call sees B's parser error count grew and emits
                // `Could not resolve external class member "v".` at A's identifier
                // (analyzer.cpp:1019). The downstream `reduce_identifier_from_base`'s
                // `Cannot find member "X" in base "Y".` (analyzer.cpp:4095/4097/4139/4141)
                // fires next because the unresolved external leaves the member typeless.
                //
                // gdls doesn't run cross-file analyzers eagerly; instead we expose per-member
                // initializer cross-references on `CrossFileQuery` and detect the cycle
                // structurally — if B's `<name>` reads `<some_const>.<our_member>` where
                // `our_member` is the file's currently-resolving member, the recursive
                // resolve would loop. Emit both diagnostics anchored at this identifier.
                if let Some(cur) = ctx.current_resolving_member.clone() {
                    // WP-R2: record_member_xref — stable marker for cross-references (docs/03 §7.5,
                    // docs/07 cite this site by marker, not a drift-prone line number; WP-RD15).
                    // Record so the inverse cross-file query (consumed by `WorkspaceXFileQuery`
                    // under the LSP) can detect cycles when this side has not yet been
                    // re-analyzed. Additive — never changes any other diagnostic or type.
                    ctx.record_member_xref(&cur, sr.file, &name);

                    let xrefs = ctx.xfile.member_initializer_xrefs(sr.file, &name);
                    // WP-RD2: `ctx.file` is `Option`; an orphan (None) never matches, so its
                    // cross-file cycle detection degrades to "no cycle" rather than mis-firing.
                    let cycles = xrefs.iter().any(|x| {
                        Some(x.target_file) == ctx.file && x.target_member.as_str() == cur
                    });
                    if cycles {
                        ctx.push_error(
                            format!(r#"Could not resolve external class member "{name}"."#),
                            identifier_id,
                        );
                        let base_class_name = ctx
                            .xfile
                            .interface(sr.file)
                            .and_then(|i| i.class_name.clone())
                            .unwrap_or_default();
                        ctx.push_error(
                            format!(r#"Cannot find member "{name}" in base "{base_class_name}"."#),
                            identifier_id,
                        );
                        ctx.set_type(identifier_id, DataType::variant());
                        return;
                    }
                }
            }
        }
        // Native tail (the analyzer.cpp:4280-4360 continuation): the chain's native root carries
        // the inherited native members. A miss here stays a SILENT Variant — unlike the pure
        // NATIVE branch below, an interface gap on a script base must never become a
        // `Cannot find member` ("never lie").
        if let Some(sr) = base.script_type.as_ref() {
            if let Some(root) = crate::script_chain::chain_native_root(ctx, sr) {
                if try_native_member(ctx, &root, &name, is_constructor, identifier_id) {
                    return;
                }
            }
        }
        ctx.set_type(identifier_id, DataType::variant());
        return;
    }

    // --- NATIVE branch (analyzer.cpp:4308-4360) --------------------------------------------------
    let native_name = base.native_type.clone();
    if !native_name.is_empty() {
        if ctx.native.class_named(&native_name).is_none() {
            // Native class not in our (possibly trimmed) DB. Degrade silently per the "never
            // crash, never lie" rule (analyzer.cpp's ClassDB always has every class).
            ctx.set_type(identifier_id, DataType::variant());
            return;
        }
        // A miss leaves the type unset — the caller (reduce_subscript) emits the
        // `Cannot find member` error for genuinely-introspectable native bases.
        let _ = try_native_member(ctx, &native_name, &name, is_constructor, identifier_id);
    }
    // CLASS / SCRIPT branches handled above; bases with no native_type and no class_node degrade
    // to Variant (the silent path Godot's `current_class`-fallback at 4030-4034 would have
    // reached in a healthy parse).
}

/// The native member arms of `reduce_identifier_from_base` (analyzer.cpp:4308-4360), shared by
/// the NATIVE branch (miss ⇒ caller's `Cannot find member` / UNSAFE_PROPERTY_ACCESS), the CLASS
/// branch's native tail (v1.0.4 #32 — same miss semantics as NATIVE), the Script branch's
/// native tail (miss ⇒ silent Variant — an interface gap must never error), and the
/// bare-identifier implicit-self walk (reduce_identifier step 3.5). Sets the identifier's
/// type/fold and returns whether a member was found.
fn try_native_member(
    ctx: &mut AnalysisContext,
    native_name: &str,
    name: &str,
    is_constructor: bool,
    identifier_id: NodeId,
) -> bool {
    // 1. Property (analyzer.cpp:4317-4326). Walk inherits chain to find the declaring class.
    if let Some(prop) = lookup_native_property(ctx, native_name, name) {
        ctx.set_type(identifier_id, prop);
        return true;
    }
    // 2. Method (analyzer.cpp:4327-4332). gdls returns the callable type (Variant + sig);
    //    the full make_callable_type lives with reduce_call.
    if native_method_exists(ctx, native_name, name) {
        let mut t = DataType {
            type_source: TypeSource::AnnotatedExplicit,
            kind: DtKind::Builtin,
            builtin_type: VariantType::Callable,
            is_constant: true,
            ..Default::default()
        };
        t.is_meta_type = false;
        ctx.set_type(identifier_id, t);
        return true;
    }
    // 3. Signal (analyzer.cpp:4333-4338).
    if native_signal_exists(ctx, native_name, name) {
        let mut t = DataType {
            type_source: TypeSource::AnnotatedExplicit,
            kind: DtKind::Builtin,
            builtin_type: VariantType::Signal,
            is_constant: true,
            ..Default::default()
        };
        t.is_meta_type = false;
        ctx.set_type(identifier_id, t);
        return true;
    }
    // 4. Enum (analyzer.cpp:4339-4343).
    if native_has_enum(ctx, native_name, name) {
        let t = crate::resolver::make_native_enum_type(ctx, name, native_name, true);
        ctx.set_type(identifier_id, t);
        return true;
    }
    // 5. Integer constant — value-in-enum (analyzer.cpp:4344-4359).
    if let Some((val, owning_enum)) = lookup_native_constant(ctx, native_name, name) {
        ctx.folds.set(identifier_id, FoldedValue::Int(val));
        if let Some(enum_name) = owning_enum {
            let t = crate::resolver::make_native_enum_type(ctx, &enum_name, native_name, false);
            ctx.set_type(identifier_id, t);
        } else {
            let mut t = DataType {
                type_source: TypeSource::AnnotatedExplicit,
                kind: DtKind::Builtin,
                builtin_type: VariantType::Int,
                is_constant: true,
                ..Default::default()
            };
            t.is_meta_type = false;
            ctx.set_type(identifier_id, t);
        }
        return true;
    }
    // 6. Constructor (`X.new` ⇒ `_init`) on a meta-type. `_init` is virtual on every Object
    //    in Godot's ClassDB but the trimmed dump omits it; synthesize a Callable so we
    //    don't false-positive "Cannot find member new" on every legitimate `X.new()`.
    if is_constructor {
        let mut t = DataType {
            type_source: TypeSource::AnnotatedExplicit,
            kind: DtKind::Builtin,
            builtin_type: VariantType::Callable,
            is_constant: true,
            ..Default::default()
        };
        t.is_meta_type = false;
        ctx.set_type(identifier_id, t);
        return true;
    }
    false
}

// --- reduce_identifier_from_base helpers -----------------------------------------------------------

/// Walk the inherits chain looking for a native property, returning its type. A property whose
/// dump entry has no setter (`setter: None`) is read-only — Godot at analyzer.cpp:4321 reads
/// the property's setter via `ClassDB::get_property_setter` and stamps `is_read_only` on the
/// resulting DataType, which `reduce_assignment` (analyzer.cpp:2920-2941) checks for the
/// `Cannot assign a new value to a read-only property.` error.
fn lookup_native_property(ctx: &AnalysisContext, native: &str, name: &str) -> Option<DataType> {
    let mut cur = Some(native.to_owned());
    while let Some(c) = cur {
        let nc = ctx.native.class_named(&c)?;
        if let Some(p) = nc
            .properties
            .iter()
            .find(|p| ctx.native.name_of(p.name) == name)
        {
            let mut dt = type_from_type_ref(ctx, &p.ty);
            dt.is_read_only = p.setter.is_none();
            return Some(dt);
        }
        cur = nc.inherits.map(|s| ctx.native.name_of(s).to_owned());
    }
    None
}

fn native_method_exists(ctx: &AnalysisContext, native: &str, name: &str) -> bool {
    let mut cur = Some(native.to_owned());
    while let Some(c) = cur {
        let Some(nc) = ctx.native.class_named(&c) else {
            return false;
        };
        if nc
            .methods
            .iter()
            .any(|m| ctx.native.name_of(m.name) == name)
        {
            return true;
        }
        cur = nc.inherits.map(|s| ctx.native.name_of(s).to_owned());
    }
    false
}

fn native_signal_exists(ctx: &AnalysisContext, native: &str, name: &str) -> bool {
    let mut cur = Some(native.to_owned());
    while let Some(c) = cur {
        let Some(nc) = ctx.native.class_named(&c) else {
            return false;
        };
        if nc
            .signals
            .iter()
            .any(|s| ctx.native.name_of(s.name) == name)
        {
            return true;
        }
        cur = nc.inherits.map(|s| ctx.native.name_of(s).to_owned());
    }
    false
}

pub(crate) fn native_has_enum(ctx: &AnalysisContext, native: &str, name: &str) -> bool {
    let mut cur = Some(native.to_owned());
    while let Some(c) = cur {
        let Some(nc) = ctx.native.class_named(&c) else {
            return false;
        };
        if nc.enums.iter().any(|e| ctx.native.name_of(e.name) == name) {
            return true;
        }
        cur = nc.inherits.map(|s| ctx.native.name_of(s).to_owned());
    }
    false
}

/// Walk the inherits chain looking for a native integer constant, returning its value + the name
/// of the enum it belongs to (if any). Mirrors `ClassDB::get_integer_constant` +
/// `ClassDB::get_integer_constant_enum` (analyzer.cpp:4346-4358).
fn lookup_native_constant(
    ctx: &AnalysisContext,
    native: &str,
    name: &str,
) -> Option<(i64, Option<String>)> {
    let mut cur = Some(native.to_owned());
    while let Some(c) = cur {
        let nc = ctx.native.class_named(&c)?;
        // Constant declared as a `constants` entry — no enum membership recorded in our DB.
        if let Some(k) = nc
            .constants
            .iter()
            .find(|k| ctx.native.name_of(k.name) == name)
        {
            return Some((k.value, None));
        }
        // Constant declared inside an enum.
        for ne in &nc.enums {
            if let Some((_, v)) = ne
                .values
                .iter()
                .find(|(sym, _)| ctx.native.name_of(*sym) == name)
            {
                return Some((*v, Some(ctx.native.name_of(ne.name).to_owned())));
            }
        }
        cur = nc.inherits.map(|s| ctx.native.name_of(s).to_owned());
    }
    None
}

/// Convert a `gd_types::TypeRef` to a [`DataType`] for property/return/param typing.
/// `TypeRef::Named` is ambiguous (builtin or class) so we probe the NativeDb to disambiguate —
/// same trick as Godot's late-binding lookup. Enum/bitfield refs mirror `type_from_property`'s
/// CLASS_IS_ENUM / CLASS_IS_BITFIELD arms (analyzer.cpp:5744-5759); typed collections stay
/// soft Variant (the typed-collection slice is deferred work — hardening them here would
/// activate container-element checks this port hasn't validated against the corpus).
fn type_from_type_ref(ctx: &AnalysisContext, ty: &gd_types::TypeRef) -> DataType {
    use gd_types::TypeRef;
    match ty {
        TypeRef::Named(sym) => {
            let raw = ctx.native.name_of(*sym);
            if let Some(vt) = builtin_variant_from_name(raw) {
                let mut t = DataType {
                    type_source: TypeSource::AnnotatedExplicit,
                    kind: DtKind::Builtin,
                    builtin_type: vt,
                    ..Default::default()
                };
                t.is_meta_type = false;
                t
            } else if ctx.native.class_named(raw).is_some() {
                let mut t = DataType {
                    type_source: TypeSource::AnnotatedExplicit,
                    kind: DtKind::Native,
                    builtin_type: VariantType::Object,
                    native_type: raw.to_owned(),
                    ..Default::default()
                };
                t.is_meta_type = false;
                t
            } else {
                DataType::variant()
            }
        }
        TypeRef::Void => DataType {
            type_source: TypeSource::AnnotatedExplicit,
            kind: DtKind::Builtin,
            builtin_type: VariantType::Nil,
            ..Default::default()
        },
        TypeRef::Enum { scope, name } => {
            // analyzer.cpp:5746-5757 (type_from_property): an INT flagged CLASS_IS_ENUM. Godot
            // probes CoreConstants FIRST — a dotted name like `Variant.Type` is a *global* enum,
            // not class-scoped — then splits `Class.Enum` into upstream's bare `make_enum_type`
            // (no value fill at this site; values are populated only by the identifier-from-base
            // constant arm). Either way the result is a VALUE, not a constant.
            let enum_name = ctx.native.name_of(*name).to_owned();
            let mut t = match scope {
                Some(s) => {
                    let scope_name = ctx.native.name_of(*s).to_owned();
                    if ctx
                        .native
                        .global_enum(&format!("{scope_name}.{enum_name}"))
                        .is_some()
                    {
                        crate::resolver::make_global_enum_type(ctx, &enum_name, &scope_name, false)
                    } else {
                        crate::resolver::make_enum_type(&enum_name, &scope_name, false)
                    }
                }
                None => crate::resolver::make_global_enum_type(ctx, &enum_name, "", false),
            };
            t.is_constant = false;
            t
        }
        // PROPERTY_USAGE_CLASS_IS_BITFIELD (analyzer.cpp:5758): "BitField[T] isn't supported
        // (yet?), use plain int."
        TypeRef::Bitfield { .. } => {
            let mut t = DataType {
                type_source: TypeSource::AnnotatedExplicit,
                kind: DtKind::Builtin,
                builtin_type: VariantType::Int,
                ..Default::default()
            };
            t.is_meta_type = false;
            t
        }
        TypeRef::Variant
        | TypeRef::TypedArray(_)
        | TypeRef::TypedDict(_, _)
        | TypeRef::Pointer(_) => DataType::variant(),
    }
}

/// Map a NativeDb builtin-type name string back to a [`VariantType`] discriminant. The dump
/// produces lowercase atomic names matching [`crate::data_type::variant_type_name`]; this is the
/// inverse mapping.
fn builtin_variant_from_name(name: &str) -> Option<VariantType> {
    use VariantType::*;
    Some(match name {
        "Nil" => Nil,
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
        // "Object" deliberately ABSENT: in dumps it names the native root class, not a builtin
        // (gdscript_parser's get_builtin_type excludes it too). Mapping it here typed
        // `instance_from_id()` & friends as kind-Builtin Object, which the object-polymorphism
        // arms reject — `obj is Node3D` false-positived. The caller's class_named fallback
        // produces the proper kind-Native Object.
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

/// Thin wrapper over [`reduce_identifier`] that honors the `can_be_builtin` flag. Godot's
/// `reduce_identifier(..., bool can_be_builtin)` errors on a standalone builtin name (analyzer.cpp:
/// 4531-4539) — the standalone-error gate joins with WP-F when its hosting context (call/subscript)
/// has full reducers; until then this just exposes the builtin metatype (matches the existing
/// behavior of [`reduce_identifier`]).
fn reduce_identifier_with_flags(ctx: &mut AnalysisContext, id: NodeId, _can_be_builtin: bool) {
    // The flag plumbing is here so future E3 slices can wire the standalone-builtin gate without
    // touching every caller; right now the underlying reduce_identifier already exposes builtins
    // unconditionally, which is what the corpus expects.
    reduce_expression(ctx, id, false);
}

// ===================================================================================================
// reduce_get_node — analyzer.cpp:3848  (v1 policy: docs/02-frontend-port.md §11)
// ===================================================================================================

/// `GDScriptAnalyzer::reduce_get_node(p_get_node)` (analyzer.cpp:3848-3870). **gdls deliberately
/// deviates from Godot here** — see docs/02-frontend-port.md §11. Until `.tscn` typing lands in
/// Phase 2, `$NodePath` / `%UniqueName` expressions yield a **permissive deferred-node type** so
/// the tool never emits a false positive on node access:
///
/// * **No `Cannot use shorthand "get_node()" notation … on a class that isn't a node.` error** —
///   Godot checks `ClassDB::is_parent_class(current_class.base_type.native_type, "Node")`. The
///   `errors/get_node_shorthand_within_non_node.gd` corpus case stays permanently on the
///   known-failure list per the v1 policy.
/// * **No `Cannot use shorthand "get_node()" notation … in a static function.` error** — Godot
///   gates on `static_context`. The `errors/get_node_shorthand_in_static_function.gd` case stays
///   on the known-failure list.
/// * **Result type is `Variant`**, not `NATIVE Node`. Per docs/02 §11 this is what makes
///   `var enemy: Node3D = $Enemy` typecheck without "incompatible assignment" warnings: Variant
///   accepts/produces any object type (`is_type_compatible` at analyzer.cpp:6315-6323), and member
///   access on Variant is dynamic. Phase 2's `.tscn`-aware typing replaces this with the precise
///   scene-derived node class.
fn reduce_get_node(ctx: &mut AnalysisContext, id: NodeId) {
    if !matches!(&ctx.node(id).kind, NodeKind::GetNode(_)) {
        return;
    }
    // WP-P10: get_node shorthand context restrictions (analyzer.cpp's `reduce_get_node` —
    // ~4870-4895). Two errors fire when `$Node` or `%Unique` is used in an inappropriate
    // context — both must be checked here because Godot's parser doesn't carry the
    // enclosing-class / enclosing-function shape that determines validity.
    //
    // (a) In a static function (no `self`): `Cannot use shorthand "get_node()" notation
    //     ("$") in a static function.`
    // (b) In a class that isn't Node-derived: `Cannot use shorthand "get_node()" notation
    //     ("$") on a class that isn't a node.`
    //
    // Godot checks the enclosing class's native ancestor against "Node" — gdls uses the
    // `nearest_native_ancestor` helper (shared with WP-N5a's @onready / @export-of-Node walk).
    let in_static_function = ctx
        .current_function
        .and_then(|fn_id| match &ctx.node(fn_id).kind {
            NodeKind::Function(f) => Some(f.is_static),
            _ => None,
        })
        .unwrap_or(false);
    if in_static_function {
        ctx.push_error(
            r#"Cannot use shorthand "get_node()" notation ("$") in a static function."#,
            id,
        );
    }

    let in_non_node_class = ctx
        .current_class
        .and_then(|cc| crate::resolver::nearest_native_ancestor(ctx, cc))
        .is_some_and(|native| !ctx.native.is_subclass_of_named(&native, "Node"));
    if in_non_node_class {
        ctx.push_error(
            r#"Cannot use shorthand "get_node()" notation ("$") on a class that isn't a node."#,
            id,
        );
    }

    // Permissive deferred-node type — see fn-doc. When the contextual checks above DID emit an
    // error the node type is effectively "no type" (Godot leaves `Variant` with the default
    // `UNDETECTED` source), which propagates into a `:=` infer's `Cannot infer the type of "X"
    // variable because the value doesn't have a set type.` companion. Otherwise we keep the
    // permissive-deferred Variant so legitimate `$Node` accesses don't false-positive infer
    // failures on classes that didn't error.
    if in_static_function || in_non_node_class {
        ctx.set_type(
            id,
            DataType {
                kind: DtKind::Variant,
                type_source: TypeSource::Undetected,
                ..Default::default()
            },
        );
    } else {
        ctx.set_type(id, DataType::variant());
    }
}

// ===================================================================================================
// reduce_await — analyzer.cpp:3053
// ===================================================================================================

/// `GDScriptAnalyzer::reduce_await(p_await)` (analyzer.cpp:3053-3087). Three things:
///
/// 1. Reduce the target. When the target is a CALL, Godot sets `p_is_await=true` so the
///    call's MISSING_AWAIT check (analyzer.cpp:3751-3758) doesn't fire on this very call. gdls
///    threads the flag through `AnalysisContext::awaiting_call` rather than an extra parameter.
/// 2. Compute the await-result type: for a `Signal` target the result is Variant (we can't
///    type a signal's payload), otherwise it's the target's type with `is_coroutine` cleared.
/// 3. (`DEBUG_ENABLED`) emit `REDUNDANT_AWAIT` (analyzer.cpp:3083-3085) when the target is
///    neither a coroutine, nor a Variant, nor a `Signal`.
fn reduce_await(ctx: &mut AnalysisContext, id: NodeId) {
    let to_await = match &ctx.node(id).kind {
        NodeKind::Await(a) => a.to_await,
        _ => return,
    };

    let Some(target) = to_await else {
        ctx.set_type(id, DataType::variant());
        return;
    };

    let target_is_call = matches!(&ctx.node(target).kind, NodeKind::Call(_));
    let prev_awaiting = ctx.awaiting_call;
    if target_is_call {
        ctx.awaiting_call = true;
    }
    reduce_expression(ctx, target, false);
    ctx.awaiting_call = prev_awaiting;

    let mut to_await_type = ctx.get_type(target).clone();

    let result_type = if to_await_type.is_hard_type()
        && to_await_type.kind == DtKind::Builtin
        && to_await_type.builtin_type == VariantType::Signal
    {
        // analyzer.cpp:3071-3073 — we can't infer the type of a signal's payload, so the result
        // is Variant with Undetected source.
        DataType {
            kind: DtKind::Variant,
            type_source: TypeSource::Undetected,
            ..Default::default()
        }
    } else {
        let mut r = to_await_type.clone();
        r.is_coroutine = false;
        r
    };

    ctx.set_type(id, result_type);

    // analyzer.cpp:3082-3086 — REDUNDANT_AWAIT fires when none of the three legitimate awaitable
    // conditions hold. Note we re-read the target's type without clearing is_coroutine.
    to_await_type = ctx.get_type(target).clone();
    let is_signal =
        to_await_type.kind == DtKind::Builtin && to_await_type.builtin_type == VariantType::Signal;
    if !to_await_type.is_coroutine && !to_await_type.is_variant() && !is_signal {
        ctx.push_warning(crate::warnings::WarningCode::RedundantAwait, &[], id);
    }
}

// ===================================================================================================
// reduce_preload — analyzer.cpp:4694
// ===================================================================================================

/// `GDScriptAnalyzer::reduce_preload(p_preload)` (analyzer.cpp:4694-4757). The path expression is
/// reduced and required to be a constant string; cross-file resource resolution (the
/// `ResourceLoader::exists` / `get_depended_shallow_script` / `type_from_variant` chain at
/// analyzer.cpp:4711-4751) is deferred to the cross-file E3 slice — for now the preload result is
/// typed as Variant so the rest of the analyzer doesn't fall over.
///
/// Errors emitted in this slice:
/// * **`Preloaded path must be a constant string.`** (analyzer.cpp:4702 / 4707) when the path is
///   not a foldable constant or the folded value isn't a String/StringName/NodePath.
///
/// Deferred:
/// * **`Preload file "X" does not exist.`** / **`has no resource loaders`** (analyzer.cpp:4719-
///   4721) — needs project-root + filesystem access; lands with the cross-file slice.
/// * **Type-from-variant of the loaded resource** (analyzer.cpp:4751) — needs cross-file parser
///   refs; for now we stay at Variant so `const P = preload("foo.gd")` doesn't error on use, just
///   degrades any subsequent `P.Member` to Variant via the subscript path.
fn reduce_preload(ctx: &mut AnalysisContext, id: NodeId) {
    let path_id = match &ctx.node(id).kind {
        NodeKind::Preload(p) => p.path,
        _ => return,
    };
    let Some(path_id) = path_id else {
        ctx.set_type(id, DataType::variant());
        return;
    };

    reduce_expression(ctx, path_id, false);

    // analyzer.cpp:4701-4707 — must be a constant string. The path is a string when the folded
    // value is a String/StringName/NodePath (the parser conflates these into FoldedValue::String;
    // see folded_from_literal). A non-foldable expression isn't constant; a non-string fold isn't
    // a path.
    let folded_path: Option<String> = match ctx.folds.get(path_id) {
        Some(crate::FoldedValue::String(s)) => Some(s.clone()),
        _ => None,
    };
    if folded_path.is_none() {
        ctx.push_error("Preloaded path must be a constant string.", path_id);
    }

    // WP-P1 cross-file: Godot's tail at analyzer.cpp:4749-4751 sets
    // `reduced_value = p_preload->resource` and types via `type_from_variant(reduced_value)` —
    // for a GDScript resource this produces a Script-kind meta type pointing at the loaded
    // script. Mirror that here via the cross-file query: when the path folds to a known file
    // (a resolved `res://…` or sibling-relative path), set the preload's type to that file's
    // Script meta. This drains `lookup_class.gd`, `out_of_order_external.gd`, and the
    // `inner_class_as_return_type` family — each names a preloaded constant whose use as a
    // type / base / extends-head needs the constant to carry a Script-kind type.
    let mut path_for_resource_typing: Option<String> = None;
    if let Some(path_str) = folded_path {
        let resolved = match ctx.file {
            // Relative paths resolve against the referring script's directory
            // (analyzer.cpp:437's relativization).
            Some(from) => ctx.xfile.resolve_path_from(from, &path_str),
            None => ctx.xfile.resolve_res_path(&path_str),
        };
        path_for_resource_typing = Some(path_str);
        if let Some(file) = resolved {
            let preload_type = DataType {
                type_source: TypeSource::AnnotatedInferred,
                kind: DtKind::Script,
                builtin_type: VariantType::Object,
                is_meta_type: true,
                is_constant: true,
                script_type: Some(crate::data_type::ScriptRef {
                    file,
                    inner: Vec::new(),
                }),
                ..Default::default()
            };
            ctx.set_type(id, preload_type);
            return;
        }
    }

    // Non-script resources: Godot types the preload by the loaded resource's class
    // (analyzer.cpp:4749-4751 via type_from_variant over the Resource). A shallow extension map
    // covers the unambiguous ones; everything else stays a soft Variant.
    if let Some(path_str) = path_for_resource_typing {
        let native_name = match path_str.rsplit('.').next() {
            Some("tscn") | Some("scn") => Some("PackedScene"),
            Some("gdshader") => Some("Shader"),
            Some("tres") | Some("res") => Some("Resource"),
            _ => None,
        };
        if let Some(n) = native_name {
            if ctx.native.class_named(n).is_some() {
                ctx.set_type(
                    id,
                    DataType {
                        type_source: TypeSource::AnnotatedInferred,
                        kind: DtKind::Native,
                        builtin_type: VariantType::Object,
                        native_type: n.to_owned(),
                        is_constant: true,
                        ..Default::default()
                    },
                );
                return;
            }
        }
    }

    // Path unresolved (`NoCrossFile`, unknown corpus path, non-string fold) ⇒ degrade to Variant.
    ctx.set_type(id, DataType::variant());
}

// ===================================================================================================
// reduce_lambda — analyzer.cpp:4667
// ===================================================================================================

/// `GDScriptAnalyzer::reduce_lambda(p_lambda)` (analyzer.cpp:4667-4685). A lambda expression is
/// always a `Callable` (`ANNOTATED_INFERRED`). v1 ports the type-stamping arm only:
///
/// * **Lambda body resolution is deferred.** Godot queues the lambda on
///   `pending_body_resolution_lambdas` (analyzer.cpp:4684) and walks it later in
///   `resolve_pending_lambda_bodies` (analyzer.cpp:1646-1671). gdls's resolver doesn't yet drain
///   that queue, so the lambda-body return-path checks (`lambda_no_return.gd`,
///   `lambda_wrong_return.gd`) and the lambda-param cycle (`lambda_cyclic_ref_param.gd`) stay on
///   the known-failure list until the pending-body pass lands.
/// * **Parameter signature resolution is also deferred.** Inside the lambda body, parameter
///   identifiers would resolve to Variant via the tail-guard — the same permissive fallback used
///   for un-ported expression kinds elsewhere — so `lambda.call(args)` and parameter mentions
///   inside the body still typecheck without false positives.
/// * **`current_lambda` tracking** (analyzer.cpp:4679-4682) is gated on `mark_lambda_use_self`
///   and the static-self check that don't apply in gdls's permissive `reduce_get_node` /
///   `reduce_self` paths, so the field isn't wired on `AnalysisContext` yet — joins WP-E3l with
///   the rest of the lambda body machinery.
fn reduce_lambda(ctx: &mut AnalysisContext, id: NodeId) {
    // analyzer.cpp:4668-4673 — Lambda is always a Callable. The make_callable_type placeholder
    // is folded inline here since we don't yet carry method-info for the callable's signature
    // (deferred to WP-E3l's full make_callable_type).
    let mut lambda_type = DataType {
        type_source: TypeSource::AnnotatedInferred,
        kind: DtKind::Builtin,
        builtin_type: VariantType::Callable,
        ..Default::default()
    };
    lambda_type.is_constant = false;
    ctx.set_type(id, lambda_type);

    // analyzer.cpp:4681 — resolve the lambda's function signature now so the deferred body pass
    // (and the immediate `make_callable_type` consumer) sees the lambda's parameter types and
    // declared return type populated. Godot passes `p_lambda` + `is_lambda=true`; gdls's
    // `resolve_function_signature` doesn't need the lambda pointer because the `_init` /
    // `_static_init` special cases gate on the function's identifier name (empty for anonymous
    // lambdas).
    if let NodeKind::Lambda(l) = &ctx.node(id).kind {
        if let Some(func_id) = l.function {
            crate::resolver::resolve_function_signature(ctx, func_id);
        }
    }

    // analyzer.cpp:4684 — queue the lambda for body resolution after the enclosing class's
    // function bodies finish. Capture `ctx.concrete_function` so the lambda's
    // static-context errors report the outer concrete function's name rather than the lambda
    // itself (Godot reaches the same value by walking `source_lambda -> parent_function`).
    // Also capture `ctx.static_context` so the body pass inherits the surrounding static-ness:
    // Godot's `resolve_function_signature` at analyzer.cpp:1749-1751 mutates the lambda's
    // `is_static = static_context` (since the `static` keyword can't appear on a lambda, the
    // surrounding scope is the source of truth). gdls's parser already inherits static-ness
    // from the enclosing function, but at class-body level — e.g. inside a `static var`
    // initializer — the parser had no current_function to inherit from, so it captures
    // `false` even though the initializer runs under `static_context = true`. Carrying the
    // captured value through the queue lets drain seed `static_context` correctly without
    // mutating the immutable parse tree.
    ctx.pending_lambda_bodies.push((
        id,
        ctx.concrete_function,
        ctx.static_context,
        ctx.suite_stack.clone(),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cross_file::NoCrossFile;
    use crate::warn_policy::{StrictSettings, WarnPolicy};
    use gd_project::{FileId, WarningConfig};
    use gd_syntax::ast::{BinaryOpNode, LiteralNode, Node, NodeKind, ParseTree, UnaryOpNode};
    use gd_types::NativeDb;

    fn mini_native() -> NativeDb {
        NativeDb::from_json(
            r#"{
                "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
                "classes": [{"name": "Object"}, {"name": "RefCounted", "inherits": "Object"}]
            }"#,
        )
        .expect("valid mini dump")
    }

    fn policy() -> WarnPolicy {
        WarnPolicy::build(&WarningConfig::default(), &StrictSettings::default())
    }

    /// Build a minimal `ParseTree` containing exactly the nodes we hand-shape, so the reducer can
    /// be exercised without going through the parser/AST guard rails. We never test the parser
    /// here — only the reducer's behavior on a known node shape.
    fn build_tree(builder: impl FnOnce(&mut ParseTree) -> NodeId) -> (ParseTree, NodeId) {
        let mut tree = ParseTree::new();
        let id = builder(&mut tree);
        (tree, id)
    }

    fn lit_int(tree: &mut ParseTree, v: i64) -> NodeId {
        tree.push(Node::new(NodeKind::Literal(LiteralNode {
            value: Literal::Int(v),
        })))
    }

    fn lit_float(tree: &mut ParseTree, v: f64) -> NodeId {
        tree.push(Node::new(NodeKind::Literal(LiteralNode {
            value: Literal::Float(v),
        })))
    }

    fn lit_str(tree: &mut ParseTree, s: &str) -> NodeId {
        tree.push(Node::new(NodeKind::Literal(LiteralNode {
            value: Literal::String(s.to_owned()),
        })))
    }

    fn lit_bool(tree: &mut ParseTree, b: bool) -> NodeId {
        tree.push(Node::new(NodeKind::Literal(LiteralNode {
            value: Literal::Bool(b),
        })))
    }

    fn reduce_one(tree: &ParseTree, id: NodeId) -> (DataType, Option<FoldedValue>) {
        let native = mini_native();
        let xfile = NoCrossFile;
        let pol = policy();
        let mut ctx = AnalysisContext::new(tree, &native, &xfile, Some(FileId::new(1)), "", &pol);
        reduce_expression(&mut ctx, id, true);
        (ctx.get_type(id).clone(), ctx.folds.get(id).cloned())
    }

    #[test]
    fn wp_rd2_orphan_file_records_none_attributed_bindings() {
        // WP-RD2: a file the index doesn't know (an `untitled:` buffer or a `.gd` outside the
        // project) is analyzed with `file = None`. EVERY recorded Binding must then carry `None`
        // for its target/callee file — "don't know" — never a colliding placeholder id. That is
        // what makes the LSP nav handlers correctly return empty for cross-script nav of such a
        // buffer instead of mis-attributing its references to whichever real script the index
        // interned first (the retired `FileId(0)` bug). Analyzing the SAME source as a *known*
        // file (`Some(..)`) attributes the in-file binding to that id — the contrast pins that the
        // `None` case above is the orphan path, not a recording that simply never fires.
        // `extends RefCounted` — `RefCounted` is in `mini_native()`, so inheritance resolves and
        // the body pass (where bindings are recorded) actually runs.
        let src = "class_name Foo\nextends RefCounted\nvar count := 0\n\
                   func helper() -> void:\n\tpass\n\
                   func go() -> void:\n\thelper()\n\tcount += 1\n";
        let tree = gd_syntax::parse(src).tree;
        let native = mini_native();
        let xfile = NoCrossFile;
        let pol = policy();

        let orphan = crate::analyze(&tree, None, "foo.gd", &native, &xfile, &pol);
        let mut orphan_bindings = 0usize;
        for b in orphan.bindings() {
            match b {
                Binding::Call { callee, .. } => {
                    assert_eq!(
                        callee.script_file(),
                        None,
                        "orphan Call binding must record a non-Script callee (don't know)"
                    );
                    orphan_bindings += 1;
                }
                Binding::Use { target_file, .. } => {
                    assert_eq!(*target_file, None, "orphan Use binding must record None");
                    orphan_bindings += 1;
                }
            }
        }
        assert!(
            orphan_bindings > 0,
            "expected the orphan analyze to record at least one binding (the in-file helper() call)"
        );

        let known = crate::analyze(&tree, Some(FileId::new(1)), "foo.gd", &native, &xfile, &pol);
        let known_attributes_in_file = known.bindings().iter().any(|b| {
            b.callee_script_file().is_some()
                || matches!(
                    b,
                    Binding::Use {
                        target_file: Some(_),
                        ..
                    }
                )
        });
        assert!(
            known_attributes_in_file,
            "the known-file analyze must attribute its in-file binding to Some(FileId) — proving \
             the orphan None above is the orphan path, not an absence of recording"
        );
    }

    #[test]
    fn literal_int_folds() {
        let (tree, id) = build_tree(|t| lit_int(t, 42));
        let (dt, fold) = reduce_one(&tree, id);
        assert_eq!(dt.builtin_type, VariantType::Int);
        assert!(dt.is_hard_type());
        assert_eq!(fold, Some(FoldedValue::Int(42)));
    }

    #[test]
    fn literal_string_folds() {
        let (tree, id) = build_tree(|t| lit_str(t, "abc"));
        let (dt, fold) = reduce_one(&tree, id);
        assert_eq!(dt.builtin_type, VariantType::String);
        assert_eq!(fold, Some(FoldedValue::String("abc".to_owned())));
    }

    #[test]
    fn unary_negate_int_folds() {
        let (tree, id) = build_tree(|t| {
            let inner = lit_int(t, 7);
            t.push(Node::new(NodeKind::UnaryOp(UnaryOpNode {
                operation: UnaryOp::Negative,
                operand: Some(inner),
            })))
        });
        let (dt, fold) = reduce_one(&tree, id);
        assert_eq!(dt.builtin_type, VariantType::Int);
        assert_eq!(fold, Some(FoldedValue::Int(-7)));
    }

    #[test]
    fn unary_not_bool_folds() {
        let (tree, id) = build_tree(|t| {
            let inner = lit_bool(t, true);
            t.push(Node::new(NodeKind::UnaryOp(UnaryOpNode {
                operation: UnaryOp::LogicNot,
                operand: Some(inner),
            })))
        });
        let (dt, fold) = reduce_one(&tree, id);
        assert_eq!(dt.builtin_type, VariantType::Bool);
        assert_eq!(fold, Some(FoldedValue::Bool(false)));
    }

    #[test]
    fn binary_add_int_folds() {
        let (tree, id) = build_tree(|t| {
            let l = lit_int(t, 2);
            let r = lit_int(t, 3);
            t.push(Node::new(NodeKind::BinaryOp(BinaryOpNode {
                operation: BinaryOp::Addition,
                left_operand: Some(l),
                right_operand: Some(r),
            })))
        });
        let (dt, fold) = reduce_one(&tree, id);
        assert_eq!(dt.builtin_type, VariantType::Int);
        assert_eq!(fold, Some(FoldedValue::Int(5)));
    }

    #[test]
    fn binary_add_mixed_int_float_folds_to_float() {
        let (tree, id) = build_tree(|t| {
            let l = lit_int(t, 2);
            let r = lit_float(t, 3.5);
            t.push(Node::new(NodeKind::BinaryOp(BinaryOpNode {
                operation: BinaryOp::Addition,
                left_operand: Some(l),
                right_operand: Some(r),
            })))
        });
        let (dt, fold) = reduce_one(&tree, id);
        assert_eq!(dt.builtin_type, VariantType::Float);
        assert_eq!(fold, Some(FoldedValue::Float(5.5)));
    }

    #[test]
    fn binary_concat_strings_folds() {
        let (tree, id) = build_tree(|t| {
            let l = lit_str(t, "a");
            let r = lit_str(t, "b");
            t.push(Node::new(NodeKind::BinaryOp(BinaryOpNode {
                operation: BinaryOp::Addition,
                left_operand: Some(l),
                right_operand: Some(r),
            })))
        });
        let (dt, fold) = reduce_one(&tree, id);
        assert_eq!(dt.builtin_type, VariantType::String);
        assert_eq!(fold, Some(FoldedValue::String("ab".to_owned())));
    }

    #[test]
    fn binary_eq_with_nil_is_bool() {
        // Godot's analyzer.cpp:3147 special-case: `1 == null` types as BOOL even though it isn't
        // constant-foldable here (because Literal::Null folds to Nil and the comparison would).
        // Use float == nil which is constant-foldable AND hits the nil-eq path.
        let (tree, id) = build_tree(|t| {
            let l = lit_int(t, 1);
            let r = t.push(Node::new(NodeKind::Literal(LiteralNode {
                value: Literal::Null,
            })));
            t.push(Node::new(NodeKind::BinaryOp(BinaryOpNode {
                operation: BinaryOp::CompEqual,
                left_operand: Some(l),
                right_operand: Some(r),
            })))
        });
        let (dt, fold) = reduce_one(&tree, id);
        assert_eq!(dt.builtin_type, VariantType::Bool);
        // Constant-fold path: 1 == nil → false.
        assert_eq!(fold, Some(FoldedValue::Bool(false)));
    }

    #[test]
    fn binary_bool_plus_bool_emits_invalid_operands_error() {
        // Godot's `core/variant/variant_op.cpp` does not register OP_ADD for BOOL+BOOL, so the
        // analyzer pushes `Invalid operands to operator +, bool and bool.` (analyzer.cpp:3130).
        // E3a's port reproduces that exact diagnostic.
        let (tree, id) = build_tree(|t| {
            let l = lit_bool(t, true);
            let r = lit_bool(t, true);
            t.push(Node::new(NodeKind::BinaryOp(BinaryOpNode {
                operation: BinaryOp::Addition,
                left_operand: Some(l),
                right_operand: Some(r),
            })))
        });
        let native = mini_native();
        let xfile = NoCrossFile;
        let pol = policy();
        let mut ctx = AnalysisContext::new(&tree, &native, &xfile, Some(FileId::new(1)), "", &pol);
        reduce_expression(&mut ctx, id, true);
        let result = ctx.finish();
        let errors: Vec<&str> = result
            .diagnostics
            .iter()
            .filter(|d| d.severity == crate::diagnostic::Severity::Error)
            .map(|d| d.message.as_str())
            .collect();
        assert_eq!(
            errors,
            vec!["Invalid operands to operator +, bool and bool."]
        );
    }

    #[test]
    fn unary_negate_bool_no_longer_folds() {
        // Godot registers OP_NEGATE only for INT and FLOAT. The pre-strict E1 cut wrongly widened
        // Bool→Int here; this regression test pins the fix: `-true` does NOT fold to `Int(-1)` but
        // instead degrades to Variant (until E2's `get_operation_type` lands the conclusive error).
        let (tree, id) = build_tree(|t| {
            let inner = lit_bool(t, true);
            t.push(Node::new(NodeKind::UnaryOp(UnaryOpNode {
                operation: UnaryOp::Negative,
                operand: Some(inner),
            })))
        });
        let (dt, fold) = reduce_one(&tree, id);
        assert!(
            dt.is_variant(),
            "unary-negate of bool must NOT widen to Int"
        );
        assert!(
            fold.is_none(),
            "unary-negate of bool must NOT record a folded value"
        );
    }

    #[test]
    fn binary_division_by_zero_does_not_panic() {
        // CLAUDE.md "never crash, never lie": 1/0 in a constant context must not panic. Godot
        // pushes "Invalid operands …" via Variant::evaluate r_valid=false; gdls degrades to Variant
        // until E2 lands the proper error.
        let (tree, id) = build_tree(|t| {
            let l = lit_int(t, 1);
            let r = lit_int(t, 0);
            t.push(Node::new(NodeKind::BinaryOp(BinaryOpNode {
                operation: BinaryOp::Division,
                left_operand: Some(l),
                right_operand: Some(r),
            })))
        });
        let (dt, fold) = reduce_one(&tree, id);
        assert!(dt.is_variant(), "division by zero degrades to Variant");
        assert!(
            fold.is_none(),
            "division by zero must not record a folded value"
        );
    }

    #[test]
    fn dictionary_dup_python_key_errors() {
        // E3b: a python-style dict with `"a": 1, "a": 2` triggers Godot's dup-key error
        // (analyzer.cpp:3831).
        let src = "func test():\n\tvar d = {\n\t\t\"a\": 1,\n\t\t\"a\": 2,\n\t}\n";
        let tree = gd_syntax::parse(src).tree;
        let native = mini_native();
        let xfile = NoCrossFile;
        let pol = policy();
        let result = crate::analyze(&tree, Some(FileId::new(1)), "", &native, &xfile, &pol);
        let errors: Vec<&str> = result
            .diagnostics
            .iter()
            .filter(|d| d.severity == crate::diagnostic::Severity::Error)
            .map(|d| d.message.as_str())
            .collect();
        assert_eq!(
            errors,
            vec![r#"Key "a" was already used in this dictionary (at line 3)."#]
        );
    }

    #[test]
    fn reduce_idempotent_via_reduced_flag() {
        // Godot's reduced flag at analyzer.cpp:2600: calling reduce_expression twice is a no-op
        // on the second call, and the result is unchanged.
        let (tree, id) = build_tree(|t| lit_int(t, 99));
        let native = mini_native();
        let xfile = NoCrossFile;
        let pol = policy();
        let mut ctx = AnalysisContext::new(&tree, &native, &xfile, Some(FileId::new(1)), "", &pol);
        reduce_expression(&mut ctx, id, true);
        let dt1 = ctx.get_type(id).clone();
        reduce_expression(&mut ctx, id, true);
        let dt2 = ctx.get_type(id).clone();
        assert_eq!(dt1.builtin_type, dt2.builtin_type);
        assert!(ctx.reduced.contains(&id));
    }

    // --- FoldedValue::Opaque (builtin named constants) ------------------------------------------

    /// A mini dump whose `builtin_classes` carry `Vector3` with typed named constants — the
    /// surface the `Opaque` fold covers. `AXIS_X` is deliberately `int`-typed: the per-constant
    /// type from the dump is what keeps integer constants on vector builtins from mis-typing as
    /// the parent builtin.
    fn vector_native() -> NativeDb {
        NativeDb::from_json(
            r#"{
                "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
                "builtin_classes": [
                    {"name": "Vector3", "constants": [
                        {"name": "UP", "type": "Vector3", "value": "Vector3(0, 1, 0)"},
                        {"name": "DOWN", "type": "Vector3", "value": "Vector3(0, -1, 0)"},
                        {"name": "ONE", "type": "Vector3", "value": "Vector3(1, 1, 1)"},
                        {"name": "AXIS_X", "type": "int", "value": "0"}
                    ]}
                ],
                "classes": [{"name": "Object"}, {"name": "RefCounted", "inherits": "Object"}]
            }"#,
        )
        .expect("valid mini dump")
    }

    fn analyze_error_messages(src: &str) -> Vec<String> {
        let tree = gd_syntax::parse(src).tree;
        let native = vector_native();
        let result = crate::analyze(
            &tree,
            Some(FileId::new(1)),
            "t.gd",
            &native,
            &NoCrossFile,
            &policy(),
        );
        result
            .diagnostics
            .iter()
            .filter(|d| d.severity == crate::diagnostic::Severity::Error)
            .map(|d| d.message.clone())
            .collect()
    }

    #[test]
    fn opaque_vector_const_times_float_no_error_and_types_vector3() {
        // The headline false positive: `Vector3.UP * 3.0` used to fold the constant as a
        // placeholder Nil and report `Invalid operands to operator *, Nil and float.`.
        let src = "extends RefCounted\nfunc go() -> void:\n\tvar _v = Vector3.UP * 3.0\n";
        assert_eq!(analyze_error_messages(src), Vec::<String>::new());

        // And the op's result type is the table's (Vector3, Float) ⇒ Vector3, still constant.
        let tree = gd_syntax::parse(src).tree;
        let native = vector_native();
        let result = crate::analyze(
            &tree,
            Some(FileId::new(1)),
            "t.gd",
            &native,
            &NoCrossFile,
            &policy(),
        );
        let mut saw_binary = false;
        for id in tree.iter_ids() {
            if matches!(tree.get(id).kind, NodeKind::BinaryOp(_)) {
                let dt = result.types.get(id);
                assert_eq!(dt.kind, DtKind::Builtin);
                assert_eq!(dt.builtin_type, VariantType::Vector3);
                assert!(
                    matches!(
                        result.folds.get(id),
                        Some(FoldedValue::Opaque(VariantType::Vector3))
                    ),
                    "valid op over opaque constants must stay a (kind-known) constant"
                );
                saw_binary = true;
            }
        }
        assert!(saw_binary);
    }

    #[test]
    fn opaque_invalid_pair_keeps_constant_error_template() {
        // Constant operands always take Godot's `Variant::evaluate r_valid=false` template, even
        // when gdls validated by type because the value is opaque.
        let src = "extends RefCounted\nfunc go() -> void:\n\tvar _v = Vector3.UP * false\n";
        assert_eq!(
            analyze_error_messages(src),
            vec!["Invalid operands to operator *, Vector3 and bool.".to_owned()]
        );
    }

    #[test]
    fn opaque_int_typed_constant_uses_declared_type() {
        // `Vector3.AXIS_X` is an int constant; typing it as the parent Vector3 would
        // false-positive `int & int`.
        let src = "extends RefCounted\nfunc go() -> void:\n\tvar _m = Vector3.AXIS_X & 1\n";
        assert_eq!(analyze_error_messages(src), Vec::<String>::new());
    }

    #[test]
    fn opaque_dict_keys_never_report_duplicate() {
        // Two distinct opaque constants used to share the placeholder Nil fold and flag a
        // phantom dup-key. An unknown value can never be *proven* a duplicate.
        let src =
            "extends RefCounted\nfunc go() -> void:\n\tvar _d = {Vector3.UP: 1, Vector3.DOWN: 2}\n";
        assert_eq!(analyze_error_messages(src), Vec::<String>::new());
    }

    #[test]
    fn opaque_binary_result_satisfies_const_contexts() {
        // `const` initializers gate on `is_reduced`; the Opaque result stamp keeps a valid op
        // over builtin constants usable as a constant expression.
        let src = "extends RefCounted\nconst SCALED = Vector3.ONE * 2.0\n";
        assert_eq!(analyze_error_messages(src), Vec::<String>::new());
    }
}
