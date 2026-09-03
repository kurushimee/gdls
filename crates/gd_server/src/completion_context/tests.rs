//! Unit tests for the cursor → [`CompletionContext`] classifier.
//!
//! Every fixture marks the cursor with a `|`; [`at`] strips it, parses + tokenizes the cleaned
//! source, and classifies at the marker's byte offset. The fixture corpus is the planning probe's
//! input set (phase-2.md), one test per in-scope context plus the arg-index, string-robustness,
//! deferred, and every-offset no-panic guarantees.

use super::*;
use gd_syntax::tokenize;

/// Parse + tokenize the source (with the `|` cursor marker removed) and classify at the marker.
/// Panics if there is not exactly one `|`.
fn at(marked: &str) -> CompletionContext {
    let byte = marked.find('|').expect("fixture needs a `|` cursor marker");
    assert_eq!(
        marked.matches('|').count(),
        1,
        "fixture must have exactly one `|`"
    );
    let src = marked.replacen('|', "", 1);
    let parsed = gd_syntax::parse(&src);
    let (tokens, _errs) = tokenize(&src);
    classify(&parsed.tree, &tokens, &parsed.comments, byte)
}

/// The text the prefix span points at, for asserting the typed prefix.
fn prefix_text(marked: &str, ctx: &CompletionContext) -> Option<String> {
    let src = marked.replacen('|', "", 1);
    ctx.prefix.map(|s| src[s.start..s.end].to_string())
}

// ===================================================================================================
// One test per in-scope context (acceptance-criteria checklist).
// ===================================================================================================

#[test]
fn attribute_trailing_dot() {
    // `local.` → member access, base captured, no prefix.
    let m = "func f():\n\tvar local = 1\n\tlocal.|";
    let ctx = at(m);
    match ctx.kind {
        CompletionKind::Attribute { base } => {
            assert!(
                base.is_some(),
                "trailing `.` should recover the base node id"
            );
        }
        other => panic!("expected Attribute, got {other:?}"),
    }
    assert_eq!(prefix_text(m, &ctx), None, "trailing dot has no prefix");
}

#[test]
fn attribute_mid_name() {
    // `local.x` → member access with prefix "x", base captured.
    let m = "func f():\n\tvar local = 1\n\tlocal.x|";
    let ctx = at(m);
    match ctx.kind {
        CompletionKind::Attribute { base } => {
            assert!(
                base.is_some(),
                "mid-name `.x` should recover the base node id"
            );
        }
        other => panic!("expected Attribute, got {other:?}"),
    }
    assert_eq!(prefix_text(m, &ctx).as_deref(), Some("x"));
}

#[test]
fn attribute_builtin_static() {
    // `Color.` → same member-access shape (BUILTIN_TYPE_STATIC is a render-time split). Classified
    // as Attribute; base captured.
    let m = "func f():\n\tColor.|";
    let ctx = at(m);
    match ctx.kind {
        CompletionKind::Attribute { base } => {
            assert!(base.is_some(), "`Color.` should recover the base node id");
        }
        other => panic!("expected Attribute for Color., got {other:?}"),
    }
}

#[test]
fn attribute_wins_inside_call() {
    // `print(foo.` → member access on `foo`, NOT a call argument: the `.` rule beats the enclosing
    // `(` rule.
    let m = "func f():\n\tprint(foo.|";
    let ctx = at(m);
    assert!(
        matches!(ctx.kind, CompletionKind::Attribute { .. }),
        "`.` inside a call must classify as Attribute, got {:?}",
        ctx.kind
    );
}

#[test]
fn identifier_bare_prefix() {
    // `spe` → identifier completion, prefix "spe".
    let m = "func f():\n\tspe|";
    let ctx = at(m);
    assert_eq!(ctx.kind, CompletionKind::Identifier);
    assert_eq!(prefix_text(m, &ctx).as_deref(), Some("spe"));
}

#[test]
fn call_arguments_empty() {
    // `print(` → call arg 0, callee "print", no prefix.
    let m = "func f():\n\tprint(|";
    let ctx = at(m);
    match &ctx.kind {
        CompletionKind::CallArguments {
            callee_name,
            arg_index,
            ..
        } => {
            assert_eq!(callee_name.as_deref(), Some("print"));
            assert_eq!(*arg_index, 0);
        }
        other => panic!("expected CallArguments, got {other:?}"),
    }
    assert_eq!(prefix_text(m, &ctx), None);
}

#[test]
fn call_arguments_with_prefix() {
    // `print(spe` → call arg 0, callee "print", prefix "spe". The CALL_ARGUMENTS context carries the
    // typed prefix (acceptance lists `print(spe` under CALL_ARGUMENTS).
    let m = "func f():\n\tprint(spe|";
    let ctx = at(m);
    match &ctx.kind {
        CompletionKind::CallArguments {
            callee_name,
            arg_index,
            ..
        } => {
            assert_eq!(callee_name.as_deref(), Some("print"));
            assert_eq!(*arg_index, 0);
        }
        other => panic!("expected CallArguments, got {other:?}"),
    }
    assert_eq!(prefix_text(m, &ctx).as_deref(), Some("spe"));
}

#[test]
fn call_arguments_second_arg() {
    // `print(1, ` → call arg 1.
    let m = "func f():\n\tprint(1, |";
    let ctx = at(m);
    match ctx.kind {
        CompletionKind::CallArguments { arg_index, .. } => assert_eq!(arg_index, 1),
        other => panic!("expected CallArguments, got {other:?}"),
    }
}

#[test]
fn call_arguments_index_is_comma_count_not_array_len() {
    // `max(1, |, 2)` — cursor in the EMPTY middle slot. Arg index must be 1 (one depth-0 comma
    // before the cursor), NOT 2 (the AST argument array has [1, 2] and would mislead). This is the
    // load-bearing acceptance criterion.
    let m = "func f():\n\tmax(1, |, 2)";
    let ctx = at(m);
    match &ctx.kind {
        CompletionKind::CallArguments {
            callee_name,
            arg_index,
            ..
        } => {
            assert_eq!(callee_name.as_deref(), Some("max"));
            assert_eq!(*arg_index, 1, "comma-count arg index, not array length");
        }
        other => panic!("expected CallArguments, got {other:?}"),
    }
}

#[test]
fn call_arguments_not_confused_by_string_paren() {
    // `print("a, b)c", |)` — the `)` and `,` inside the string literal must NOT break the scan. The
    // cursor is at arg index 1 (one real comma, after the string).
    let m = "func f():\n\tprint(\"a, b)c\", |)";
    let ctx = at(m);
    match &ctx.kind {
        CompletionKind::CallArguments {
            callee_name,
            arg_index,
            ..
        } => {
            assert_eq!(callee_name.as_deref(), Some("print"));
            assert_eq!(*arg_index, 1, "in-string `,`/`)` must not affect arg index");
        }
        other => panic!("expected CallArguments despite in-string brackets, got {other:?}"),
    }
}

#[test]
fn call_arguments_multiline() {
    // A call split across lines: standalone tokenization emits Newline/Indent inside the parens, and
    // the comma counting must skip them. Cursor on the third line is arg index 1.
    let m = "func f():\n\tmax(\n\t\t1,\n\t\t|";
    let ctx = at(m);
    match ctx.kind {
        CompletionKind::CallArguments { arg_index, .. } => {
            assert_eq!(arg_index, 1, "layout tokens must not affect arg index");
        }
        other => panic!("expected CallArguments across lines, got {other:?}"),
    }
}

#[test]
fn annotation_name() {
    // `@expo` → annotation-name completion, prefix is the `@expo` token text.
    let m = "@expo|";
    let ctx = at(m);
    assert_eq!(ctx.kind, CompletionKind::Annotation);
    assert_eq!(prefix_text(m, &ctx).as_deref(), Some("@expo"));
}

#[test]
fn annotation_arguments() {
    // `@export_range(` → annotation argument list, not a call.
    let m = "@export_range(|";
    let ctx = at(m);
    match &ctx.kind {
        CompletionKind::AnnotationArguments {
            annotation_name,
            arg_index,
        } => {
            assert_eq!(annotation_name.as_deref(), Some("@export_range"));
            assert_eq!(*arg_index, 0);
        }
        other => panic!("expected AnnotationArguments, got {other:?}"),
    }
}

#[test]
fn type_name_var_hint() {
    // `var t: Vec` → type-name completion, prefix "Vec".
    let m = "func f():\n\tvar t: Vec|";
    let ctx = at(m);
    assert_eq!(ctx.kind, CompletionKind::TypeName);
    assert_eq!(prefix_text(m, &ctx).as_deref(), Some("Vec"));
}

#[test]
fn type_name_var_hint_empty() {
    // `var t: ` (trailing space, anchor is `:`) → type-name completion, no prefix.
    let m = "func f():\n\tvar t: |";
    let ctx = at(m);
    assert_eq!(ctx.kind, CompletionKind::TypeName);
    assert_eq!(prefix_text(m, &ctx), None);
}

#[test]
fn class_body_var_inline_type_offers_get_set() {
    // Class-body `var x: <cursor>` (no trailing newline) is Godot's
    // COMPLETION_PROPERTY_DECLARATION_OR_TYPE (`gdscript_parser.cpp:1241`): types PLUS the `get`/`set`
    // accessor keywords. Both the trailing-colon and the mid-type-name forms classify there.
    for m in ["var x: |", "var x: Vec|"] {
        let ctx = at(m);
        assert_eq!(
            ctx.kind,
            CompletionKind::PropertyDeclarationOrType,
            "`{m}` (class-body inline type) must be PropertyDeclarationOrType"
        );
    }
}

#[test]
fn func_local_var_inline_type_is_plain_type_name() {
    // A FUNCTION-LOCAL `var x: <cursor>` is `parse_variable(_, p_allow_property=false)`
    // (`gdscript_parser.cpp:2032`): no property path, so types ONLY — plain TypeName, never get/set.
    for m in ["func f():\n\tvar x: |", "func f():\n\tvar x: Vec|"] {
        let ctx = at(m);
        assert_eq!(
            ctx.kind,
            CompletionKind::TypeName,
            "`{m}` (function-local inline type) must stay plain TypeName"
        );
    }
}

#[test]
fn static_func_local_var_inline_type_is_plain_type_name() {
    // A `static func` body is still `parse_variable(_, p_allow_property=false)` — the leading
    // `static` keyword does not change the function-local scope, so its inner `var x: <cursor>`
    // stays plain TypeName (never get/set). The nested-block form must hold too.
    for m in [
        "static func f():\n\tvar x: |",
        "static func f():\n\tvar x: Vec|",
        "static func f():\n\tif true:\n\t\tvar x: |",
    ] {
        let ctx = at(m);
        assert_eq!(
            ctx.kind,
            CompletionKind::TypeName,
            "`{m}` (static-function-local inline type) must stay plain TypeName"
        );
    }
}

#[test]
fn static_class_body_var_inline_type_offers_get_set() {
    // A class-body `static var x: <cursor>` is still a class member (`p_allow_property=true`), so it
    // reaches COMPLETION_PROPERTY_DECLARATION_OR_TYPE — types PLUS get/set.
    for m in ["static var x: |", "static var x: Vec|"] {
        let ctx = at(m);
        assert_eq!(
            ctx.kind,
            CompletionKind::PropertyDeclarationOrType,
            "`{m}` (static class-body inline type) must be PropertyDeclarationOrType"
        );
    }
}

#[test]
fn type_name_or_void_return() {
    // `func f() -> ` → return-type completion.
    let m = "func f() -> |";
    let ctx = at(m);
    assert_eq!(ctx.kind, CompletionKind::TypeNameOrVoid);
}

#[test]
fn inherit_type() {
    // `extends Nod` → inherit-type completion, prefix "Nod".
    let m = "extends Nod|";
    let ctx = at(m);
    assert_eq!(ctx.kind, CompletionKind::InheritType);
    assert_eq!(prefix_text(m, &ctx).as_deref(), Some("Nod"));
}

#[test]
fn subscript_index() {
    // `d[` → index subscript.
    let m = "func f():\n\tvar d = {}\n\td[|";
    let ctx = at(m);
    assert_eq!(ctx.kind, CompletionKind::Subscript);
}

#[test]
fn assign_rhs() {
    // `speed = ` (trailing space, AST collapses to Suite) → assignment RHS via the token anchor `=`.
    let m = "func f():\n\tspeed = |";
    let ctx = at(m);
    assert_eq!(ctx.kind, CompletionKind::Assign);
}

#[test]
fn super_method() {
    // `super.` → super-method completion.
    let m = "func f():\n\tsuper.|";
    let ctx = at(m);
    assert_eq!(ctx.kind, CompletionKind::SuperMethod);
}

#[test]
fn override_method() {
    // `func _re` at class-body statement start → override-method completion, prefix "_re".
    let m = "func _re|";
    let ctx = at(m);
    assert_eq!(
        ctx.kind,
        CompletionKind::OverrideMethod { is_static: false }
    );
    assert_eq!(prefix_text(m, &ctx).as_deref(), Some("_re"));
}

/// #509: `_` lexes as `TokenKind::Underscore`, not an identifier, so the prefix-anchored block
/// never saw it — the override list appeared on `func `, vanished on `func _`, and came back on
/// `func _r`. `_` is the first character of every Godot virtual, so that was the one prefix a user
/// reaching for an override always types. Godot opens `COMPLETION_OVERRIDE_METHOD` before it
/// consumes the name at all, so it completes here too.
#[test]
fn a_lone_underscore_after_func_is_still_an_override_position() {
    let m = "func _|";
    let ctx = at(m);
    assert_eq!(
        ctx.kind,
        CompletionKind::OverrideMethod { is_static: false }
    );
    // The `_` is the prefix, so accepting an item REPLACES it rather than producing `func __ready`.
    assert_eq!(prefix_text(m, &ctx).as_deref(), Some("_"));

    // Inside an inner class too — the position test is "statement start in a class body".
    let inner = "class I:\n\textends Node\n\tfunc _|";
    assert_eq!(
        at(inner).kind,
        CompletionKind::OverrideMethod { is_static: false }
    );
}

/// The scoping half of #509: admitting `_` at `func _` must not turn it into a word token
/// everywhere. `_` is legal in several positions for entirely unrelated reasons, and none of them
/// changes.
#[test]
fn a_lone_underscore_elsewhere_is_unchanged() {
    for m in [
        // Godot's match wildcard — an identifier list here would be nonsense.
        "func f(x):\n\tmatch x:\n\t\t_|",
        // Declaration names the user is inventing (`COMPLETION_DECLARATION`, a bare `break`).
        "var _|",
        "const _|",
        "class _|",
        "signal _|",
        // A named lambda in expression position is not an override, with `_` as with any name.
        "func f():\n\tvar g = func _|",
    ] {
        assert_ne!(
            at(m).kind,
            CompletionKind::OverrideMethod { is_static: false },
            "`_` must not open an override position in {m:?}"
        );
    }
}

#[test]
fn type_attribute_is_distinct_from_instance_attribute() {
    // `var x: Foo.` → member access ON A TYPE: nested types/enums/constants of `Foo`, NOT `Foo`'s
    // instance members. Must be TypeAttribute, not Attribute (a confident-wrong otherwise: the
    // renderer would offer the wrong member set).
    let m = "func f():\n\tvar x: Foo.|";
    let ctx = at(m);
    assert!(
        matches!(ctx.kind, CompletionKind::TypeAttribute { .. }),
        "`var x: Foo.` must be TypeAttribute, got {:?}",
        ctx.kind
    );
}

#[test]
fn type_attribute_mid_name() {
    // `var x: Foo.Ba` → still TypeAttribute, prefix "Ba".
    let m = "func f():\n\tvar x: Foo.Ba|";
    let ctx = at(m);
    assert!(
        matches!(ctx.kind, CompletionKind::TypeAttribute { .. }),
        "got {:?}",
        ctx.kind
    );
    assert_eq!(prefix_text(m, &ctx).as_deref(), Some("Ba"));
}

#[test]
fn property_accessor_method_is_not_assign() {
    // `var x: int:` then `get = ` / `set = ` binds a getter/setter by METHOD NAME — class methods are
    // wanted, not an arbitrary RHS expression. Must be PropertyMethod, not Assign.
    for accessor in ["get", "set"] {
        let m = format!("var x: int:\n\t{accessor} = |");
        let ctx = at(&m);
        assert_eq!(
            ctx.kind,
            CompletionKind::PropertyMethod,
            "`{accessor} = ` must be PropertyMethod"
        );
    }
}

#[test]
fn bare_accessor_keyword_is_property_accessor() {
    // `var x: int:\n\t<partial>` at the accessor-keyword position offers the `get`/`set` keywords
    // (Godot COMPLETION_PROPERTY_DECLARATION). Both the first accessor and a second one (after a
    // completed `get:` body) classify as PropertyAccessor.
    for m in [
        "var x: int:\n\t|",
        "var x: int:\n\tg|",
        "var x: int:\n\ts|",
        "var x: int:\n\tget:\n\t\treturn 0\n\t|",
        "var x: int:\n\tget:\n\t\treturn 0\n\ts|",
    ] {
        let ctx = at(m);
        assert_eq!(
            ctx.kind,
            CompletionKind::PropertyAccessor,
            "`{m}` must be PropertyAccessor"
        );
    }
}

#[test]
fn accessor_body_expression_is_not_property_accessor() {
    // INSIDE an accessor body (`get:\n\t\tprin|`, `set(v):\n\t\tv|`) the cursor is in the body's
    // Function/Suite, NOT at the keyword position — it is a plain Identifier context, never
    // PropertyAccessor. Guards the AST barrier (a Function/Suite between the cursor and the
    // property Variable rejects the match).
    for m in [
        "var x: int:\n\tget:\n\t\tprin|",
        "var x: int:\n\tset(v):\n\t\tv|",
    ] {
        let ctx = at(m);
        assert_ne!(
            ctx.kind,
            CompletionKind::PropertyAccessor,
            "an accessor-body expression `{m}` must NOT be PropertyAccessor"
        );
    }
}

#[test]
fn ordinary_body_word_is_not_property_accessor() {
    // A partial word at a function-body / class-body statement start (not an accessor block) must
    // never be PropertyAccessor — the property-style Variable signal is absent there.
    for m in ["func foo():\n\tg|", "var y := 1\nfunc foo():\n\tg|"] {
        let ctx = at(m);
        assert_ne!(
            ctx.kind,
            CompletionKind::PropertyAccessor,
            "an ordinary-body word `{m}` must NOT be PropertyAccessor"
        );
    }
}

#[test]
fn get_call_is_not_property_method() {
    // `x = get(` is a *call* to a `get` method, NOT a property accessor (the `get` follows `=`, not a
    // line start). Must not be PropertyMethod.
    let m = "func f():\n\tx = get(|";
    let ctx = at(m);
    assert!(
        matches!(ctx.kind, CompletionKind::CallArguments { .. }),
        "`get(` mid-expression is a call, got {:?}",
        ctx.kind
    );
}

#[test]
fn named_lambda_is_not_override_method() {
    // `var g = func na` is a named LAMBDA in expression position, NOT a method override (the `func`
    // follows `=`, not a statement start). Must not be OverrideMethod.
    let m = "func f():\n\tvar g = func na|";
    let ctx = at(m);
    assert_ne!(
        ctx.kind,
        CompletionKind::OverrideMethod { is_static: false },
        "a named lambda must not classify as OverrideMethod"
    );
}

// ===================================================================================================
// Base-expression / callee extraction asserted explicitly.
// ===================================================================================================

#[test]
fn attribute_base_node_is_the_base_expression() {
    // The recovered base node id must be the `local` identifier node (an Identifier whose text is
    // "local"), proving base extraction, not just "some node".
    let src = "func f():\n\tvar local = 1\n\tlocal.";
    let parsed = gd_syntax::parse(src);
    let (tokens, _e) = tokenize(src);
    let ctx = classify(&parsed.tree, &tokens, &parsed.comments, src.len());
    let CompletionKind::Attribute { base: Some(base) } = ctx.kind else {
        panic!("expected Attribute with a base, got {:?}", ctx.kind);
    };
    match &parsed.tree.get(base).kind {
        NodeKind::Identifier(id) => assert_eq!(id.name, "local"),
        other => panic!("base should be the `local` Identifier, got {other:?}"),
    }
}

#[test]
fn call_arguments_callee_node_recovered_when_ast_survives() {
    // `max(1, , 2)` keeps a Call node, so the callee node id is recoverable (an Identifier "max").
    let src = "func f():\n\tmax(1, , 2)";
    let cursor = src.find(", , ").unwrap() + 2; // empty middle slot
    let parsed = gd_syntax::parse(src);
    let (tokens, _e) = tokenize(src);
    let ctx = classify(&parsed.tree, &tokens, &parsed.comments, cursor);
    let CompletionKind::CallArguments {
        callee,
        callee_name,
        ..
    } = ctx.kind
    else {
        panic!("expected CallArguments, got {:?}", ctx.kind);
    };
    assert_eq!(callee_name.as_deref(), Some("max"));
    let callee = callee.expect("max( keeps a Call node, callee id should be recovered");
    match &parsed.tree.get(callee).kind {
        NodeKind::Identifier(id) => assert_eq!(id.name, "max"),
        other => panic!("callee should be the `max` Identifier, got {other:?}"),
    }
}

// ===================================================================================================
// Deferred (M11) contexts — never misclassified as member/identifier.
// ===================================================================================================

#[test]
fn deferred_node_path_dollar() {
    // `$Player` → deferred node path, not an identifier/member.
    let m = "func f():\n\t$Player|";
    let ctx = at(m);
    assert_eq!(
        ctx.kind,
        CompletionKind::Deferred(DeferredReason::NodePath),
        "$ node path must be Deferred"
    );
}

#[test]
fn deferred_node_path_dollar_slash() {
    // `$Player/Spr` (multi-segment) → still deferred node path.
    let m = "func f():\n\t$Player/Spr|";
    let ctx = at(m);
    assert_eq!(ctx.kind, CompletionKind::Deferred(DeferredReason::NodePath));
}

#[test]
fn deferred_unique_node_path_percent() {
    // `%Health` → deferred unique node path.
    let m = "func f():\n\t%Health|";
    let ctx = at(m);
    assert_eq!(
        ctx.kind,
        CompletionKind::Deferred(DeferredReason::UniqueNodePath)
    );
}

#[test]
fn infix_modulo_percent_is_not_a_unique_node_path() {
    // `x % ` and `x % yy` are infix modulo, NOT a `%unique` node path — they must classify as a
    // normal identifier/expression context, never Deferred(UniqueNodePath) (which would fire an
    // empty completion since `%` is a trigger char). The legitimate prefix `= %Health` must stay
    // Deferred. (#94 FIX 2.)
    let bare_op = at("func f():\n\tvar y = x % |");
    assert_ne!(
        bare_op.kind,
        CompletionKind::Deferred(DeferredReason::UniqueNodePath),
        "`x % ` is modulo, not a unique node path: {:?}",
        bare_op.kind
    );

    let partial = at("func f():\n\tvar z = x % yy|");
    assert_ne!(
        partial.kind,
        CompletionKind::Deferred(DeferredReason::UniqueNodePath),
        "`x % yy` is modulo, not a unique node path: {:?}",
        partial.kind
    );

    // The prefix `%unique` sigil case must be preserved: `= %Hea` is still a unique node path.
    let prefix = at("func f():\n\tvar b = %Hea|");
    assert_eq!(
        prefix.kind,
        CompletionKind::Deferred(DeferredReason::UniqueNodePath),
        "`= %Hea` is a unique node path (prefix sigil): {:?}",
        prefix.kind
    );
}

#[test]
fn deferred_resource_path_load() {
    // `load("res://` → deferred resource path (inside a `load(...)` argument).
    let m = "func f():\n\tload(\"res://x|";
    let ctx = at(m);
    assert_eq!(
        ctx.kind,
        CompletionKind::Deferred(DeferredReason::ResourcePath),
        "load() path must be Deferred"
    );
}

#[test]
fn deferred_resource_path_preload() {
    // `preload("res://` → deferred resource path.
    let m = "func f():\n\tpreload(\"res://x|";
    let ctx = at(m);
    assert_eq!(
        ctx.kind,
        CompletionKind::Deferred(DeferredReason::ResourcePath)
    );
}

// --- M11 P3: string-form node paths (`get_node`/`get_node_or_null`/`NodePath`) ---

#[test]
fn deferred_node_path_get_node_string() {
    // `get_node("A/B/Sp|")` → a node-path string, classified `Deferred(NodePath)` (NOT a generic
    // CallArguments / ResourcePath). The cursor sits inside the string literal.
    let ctx = at("func f():\n\tget_node(\"A/B/Sp|\")");
    assert_eq!(
        ctx.kind,
        CompletionKind::Deferred(DeferredReason::NodePath),
        "get_node(\"…\") is a node path: {:?}",
        ctx.kind
    );
}

#[test]
fn deferred_node_path_get_node_or_null_and_nodepath() {
    assert_eq!(
        at("func f():\n\tget_node_or_null(\"Player|\")").kind,
        CompletionKind::Deferred(DeferredReason::NodePath),
        "get_node_or_null(\"…\") is a node path"
    );
    assert_eq!(
        at("func f():\n\tNodePath(\"Player|\")").kind,
        CompletionKind::Deferred(DeferredReason::NodePath),
        "NodePath(\"…\") is a node path"
    );
}

/// Splice an accepted item's text over the captured prefix span and return the post-edit source —
/// the corruption oracle. With `prefix = None` (zero-width), the insert lands at the cursor.
fn splice(marked: &str, insert: &str) -> String {
    let byte = marked.find('|').unwrap();
    let src = marked.replacen('|', "", 1);
    let ctx = at(marked);
    let (start, end) = ctx.prefix.map_or((byte, byte), |s| (s.start, s.end));
    format!("{}{}{}", &src[..start], insert, &src[end..])
}

/// The path is always the FIRST argument: a string in a 2nd+ argument slot
/// (`get_node(foo, "Bar|")`) is NOT a node path — it must fall through to the normal call context,
/// not fire an empty/wrong node-path completion.
#[test]
fn string_node_path_only_fires_in_first_argument() {
    let ctx = at("func f():\n\tget_node(foo, \"Bar|\")");
    assert_ne!(
        ctx.kind,
        CompletionKind::Deferred(DeferredReason::NodePath),
        "a 2nd-arg string is not a node path: {:?}",
        ctx.kind
    );
    // `load(x, "y")` likewise — the resource path is arg 0 only.
    let ctx2 = at("func f():\n\tload(x, \"y|\")");
    assert_ne!(
        ctx2.kind,
        CompletionKind::Deferred(DeferredReason::ResourcePath),
        "a 2nd-arg string is not a resource path: {:?}",
        ctx2.kind
    );
}

#[test]
fn get_node_bare_identifier_arg_is_not_a_node_path() {
    // `get_node(pa|)` — a BARE identifier arg (passing a NodePath/String variable), cursor NOT in a
    // string — must fall through to normal identifier completion, NOT a node-path context (which
    // would render an empty list and suppress completing the variable). Regression for the phase-3
    // review's MEDIUM finding.
    for call in ["get_node(pa|)", "get_node_or_null(pa|)", "NodePath(pa|)"] {
        let ctx = at(&format!("func f():\n\t{call}"));
        assert_ne!(
            ctx.kind,
            CompletionKind::Deferred(DeferredReason::NodePath),
            "bare-identifier arg in {call:?} must not be a node path: {:?}",
            ctx.kind
        );
    }
    // But the in-string form still IS a node path.
    let in_str = at("func f():\n\tget_node(\"pa|\")");
    assert_eq!(
        in_str.kind,
        CompletionKind::Deferred(DeferredReason::NodePath),
        "in-string get_node arg is still a node path: {:?}",
        in_str.kind
    );
}

/// THE corruption guard for string-form node paths: the prefix span covers EXACTLY the last
/// `/`-segment, so accepting an item rewrites only that segment — never the whole path, never quotes.
#[test]
fn string_node_path_prefix_replaces_only_last_segment() {
    let marked = "func f():\n\tget_node(\"A/B/Sp|\")";
    let ctx = at(marked);
    assert_eq!(ctx.kind, CompletionKind::Deferred(DeferredReason::NodePath));
    assert_eq!(prefix_text(marked, &ctx).as_deref(), Some("Sp"));
    assert_eq!(
        splice(marked, "Sprite2D"),
        "func f():\n\tget_node(\"A/B/Sprite2D\")"
    );
}

/// The resource-path string prefix spans the WHOLE typed content (the renderer inserts the full
/// `res://` path), so the splice replaces scheme + path wholesale — correct for any typed amount.
#[test]
fn string_resource_path_prefix_spans_whole_content() {
    let marked = "func f():\n\tload(\"res://a/b/fo|\")";
    let ctx = at(marked);
    assert_eq!(
        ctx.kind,
        CompletionKind::Deferred(DeferredReason::ResourcePath)
    );
    // The prefix is the entire content `res://a/b/fo`, not just the `fo` segment.
    assert_eq!(prefix_text(marked, &ctx).as_deref(), Some("res://a/b/fo"));
    // Accepting the full path replaces the whole partial → the canonical literal.
    assert_eq!(
        splice(marked, "res://a/b/foo.gd"),
        "func f():\n\tload(\"res://a/b/foo.gd\")"
    );
}

/// CORRUPTION GUARD (advisor): a PARTIAL scheme `load("re|")` must NOT drop `res://`. The prefix
/// spans the whole `re`, so inserting the full `res://src/foo.gd` replaces it → a valid literal,
/// never `load("src/foo.gd")` (scheme dropped) or `load("reres://…")` (doubled).
#[test]
fn resource_path_partial_scheme_does_not_drop_res() {
    for marked in [
        "func f():\n\tload(\"|\")",
        "func f():\n\tload(\"re|\")",
        "func f():\n\tload(\"res://|\")",
    ] {
        let ctx = at(marked);
        assert_eq!(
            ctx.kind,
            CompletionKind::Deferred(DeferredReason::ResourcePath),
            "{marked:?}"
        );
        assert_eq!(
            splice(marked, "res://src/foo.gd"),
            "func f():\n\tload(\"res://src/foo.gd\")".to_string(),
            "accepting the full path must yield the canonical literal for {marked:?}"
        );
    }
}

/// At the very start of an empty string argument the prefix is empty (a zero-width insertion).
#[test]
fn string_node_path_empty_prefix_at_open_quote() {
    let marked = "func f():\n\tget_node(\"|\")";
    let ctx = at(marked);
    assert_eq!(ctx.kind, CompletionKind::Deferred(DeferredReason::NodePath));
    match ctx.prefix {
        None => {}
        Some(s) => assert_eq!(s.start, s.end, "empty-argument prefix must be zero-width"),
    }
    assert_eq!(
        splice(marked, "Health"),
        "func f():\n\tget_node(\"Health\")"
    );
}

// --- M11 P3 corruption regressions (fusion review): 3 doubling/quote-eating bugs ---

/// BUG 1: a BARE quoted segment `$"Sp|"` puts the cursor inside a string-literal token, where the
/// old `prefix_at` returned `None` → a zero-width edit that DOUBLED the inserted name (`$"SpSprite"`).
/// The prefix must be the in-string `Sp` segment so the splice yields `$"Sprite"`. Multibyte too.
#[test]
fn bare_quoted_segment_prefix_does_not_double() {
    let m = "func f():\n\t$\"Sp|\"";
    assert_eq!(prefix_text(m, &at(m)).as_deref(), Some("Sp"));
    assert_eq!(splice(m, "Sprite"), "func f():\n\t$\"Sprite\"");
    // A committed earlier segment + a quoted partial: `$Player/"Sp|"`.
    let m2 = "func f():\n\t$Player/\"Sp|\"";
    assert_eq!(splice(m2, "Sprite"), "func f():\n\t$Player/\"Sprite\"");
    // Multibyte segment inside the quotes.
    let m3 = "func f():\n\t$\"Сп|\"";
    assert_eq!(splice(m3, "Спрайт"), "func f():\n\t$\"Спрайт\"");
}

/// BUG 2: a cursor AFTER the closing quote (`get_node("abc"|)`) must NOT let the edit span swallow
/// the closing quote. The cursor is not strictly inside the string, so the prefix is `None` (a
/// zero-width edit at the cursor) — the renderer additionally offers nothing there (see the wire
/// test `node_path_after_closing_quote_is_empty`). The quote is preserved either way.
#[test]
fn cursor_after_closing_quote_keeps_the_quote() {
    let m = "func f():\n\tget_node(\"abc\"|)";
    let ctx = at(m);
    // The prefix must NOT cover the closing quote (no `abc"` span).
    assert_ne!(
        prefix_text(m, &ctx).as_deref(),
        Some("abc\""),
        "the edit span must never include the closing quote"
    );
    // Whatever the prefix, it is empty/at the cursor — the closing quote stays in the source.
    let edited = splice(m, "");
    assert!(
        edited.contains("\"abc\""),
        "the terminated string keeps both quotes; got {edited:?}"
    );
}

/// BUG 3: a `%` in a resource path (`load("res://dir/a%b|")`) — `%` is a legal `res://` filename
/// byte and must NOT be a segment boundary. With the whole-content ResourcePath span (the renderer
/// inserts the full path), the `%` is just part of the content; accepting the full path replaces it
/// wholesale — no `a%` doubling.
#[test]
fn resource_path_percent_in_segment_does_not_double() {
    let m = "func f():\n\tload(\"res://dir/a%b|\")";
    let ctx = at(m);
    assert_eq!(
        ctx.kind,
        CompletionKind::Deferred(DeferredReason::ResourcePath)
    );
    // The prefix is the WHOLE content `res://dir/a%b` (no segment split for resource paths).
    assert_eq!(prefix_text(m, &ctx).as_deref(), Some("res://dir/a%b"));
    assert_eq!(
        splice(m, "res://dir/a%b.gd"),
        "func f():\n\tload(\"res://dir/a%b.gd\")"
    );
}

// ===================================================================================================
// No-panic robustness: classify at EVERY byte offset of every fixture (and existing corpus).
// ===================================================================================================

/// Every marked fixture used above, with the marker removed — the curated corpus.
const FIXTURES: &[&str] = &[
    "func f():\n\tvar local = 1\n\tlocal.",
    "func f():\n\tvar local = 1\n\tlocal.x",
    "func f():\n\tColor.",
    "func f():\n\tprint(foo.",
    "func f():\n\tspe",
    "func f():\n\tprint(",
    "func f():\n\tprint(spe",
    "func f():\n\tprint(1, ",
    "func f():\n\tmax(1, , 2)",
    "func f():\n\tprint(\"a, b)c\", )",
    "func f():\n\tmax(\n\t\t1,\n\t\t",
    "@expo",
    "@export_range(",
    "func f():\n\tvar t: Vec",
    "func f():\n\tvar t: ",
    "func f() -> ",
    "extends Nod",
    "func f():\n\tvar d = {}\n\td[",
    "func f():\n\tspeed = ",
    "func f():\n\tsuper.",
    "func _re",
    "func f():\n\t$Player",
    "func f():\n\t$Player/Spr",
    "func f():\n\t%Health",
    "func f():\n\tload(\"res://x",
    "func f():\n\tpreload(\"res://x",
    // M11 P3: string-form node paths + deeper / quoted / multibyte forms (corruption regressions).
    "func f():\n\tget_node(\"A/B/Sp",
    "func f():\n\tget_node_or_null(\"Player",
    "func f():\n\tNodePath(\"%Uniq",
    "func f():\n\t$A/B/",
    "func f():\n\t%Uniq/child",
    "func f():\n\t$\"Sp",
    "func f():\n\t$Player/\"Sp",
    "func f():\n\tload(\"res://dir/a%b",
    "func f():\n\tget_node(\"abc\")",
    // A few pathological / mixed forms.
    "",
    "\t",
    "(((",
    ".",
    "@",
    "func f():\n\tx.y.z.",
    "var a: Array[",
    "func f():\n\tif x == ",
    "match x:\n\t",
    "func f(a, b, ",
    "\u{0}",
    "🎮.",
    "func f():\n\tvar x: Foo.",
    "func f():\n\tvar x: Foo.Ba",
    "var x: int:\n\tget = ",
    "var x: int:\n\tset = ",
    "func f():\n\tvar g = func na",
    "func f():\n\tx = get(",
];

#[test]
fn classify_never_panics_at_every_offset_of_every_fixture() {
    for src in FIXTURES {
        let parsed = gd_syntax::parse(src);
        let (tokens, _errs) = tokenize(src);
        // Inclusive of 0 and len, plus one past, plus a wildly-out-of-range offset — all must clamp
        // to a well-defined result rather than panic.
        for byte in 0..=src.len() + 1 {
            let _ = classify(&parsed.tree, &tokens, &parsed.comments, byte);
        }
        let _ = classify(&parsed.tree, &tokens, &parsed.comments, usize::MAX);
    }
}

#[test]
fn classify_never_panics_on_existing_fuzz_corpus() {
    // Sweep a sample of the checked-in fuzz corpus (parser/analyzer inputs) at every offset. The
    // corpus lives at <workspace>/fuzz/corpus/{parse,analyze}; resolve from CARGO_MANIFEST_DIR.
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let corpus_root = manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|ws| ws.join("fuzz").join("corpus"));
    let Some(root) = corpus_root else {
        return;
    };
    if !root.exists() {
        return; // corpus not vendored in this checkout — skip rather than fail.
    }
    let mut checked = 0usize;
    for sub in ["parse", "analyze"] {
        let dir = root.join(sub);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten().take(60) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("gd") {
                continue;
            }
            let Ok(src) = std::fs::read_to_string(&path) else {
                continue;
            };
            let parsed = gd_syntax::parse(&src);
            let (tokens, _errs) = tokenize(&src);
            // Step a few bytes at a time to keep this fast over the whole corpus.
            let mut byte = 0;
            while byte <= src.len() {
                let _ = classify(&parsed.tree, &tokens, &parsed.comments, byte);
                byte += 1;
            }
            checked += 1;
        }
    }
    // Sanity: we actually exercised some corpus files (guards a silently-wrong path).
    assert!(checked > 0, "no fuzz-corpus .gd files were classified");
}

// ===================================================================================================
// A handful of taxonomy guards (no misclassification of common shapes).
// ===================================================================================================

#[test]
fn dict_literal_is_not_subscript() {
    // `{` opens a dictionary literal — a key position (identifiers), NOT a subscript.
    let m = "func f():\n\tvar d = {|";
    let ctx = at(m);
    assert_ne!(
        ctx.kind,
        CompletionKind::Subscript,
        "dictionary literal `{{` must not be a Subscript"
    );
}

#[test]
fn func_param_list_is_not_call_arguments() {
    // `func f(a, ` is a parameter list, not a call — must not be CallArguments.
    let m = "func f(a, |";
    let ctx = at(m);
    assert!(
        !matches!(ctx.kind, CompletionKind::CallArguments { .. }),
        "func parameter list must not classify as CallArguments, got {:?}",
        ctx.kind
    );
}

#[test]
fn comparison_is_not_assign() {
    // `if x == ` uses `==` (comparison), which must NOT be read as an assignment RHS.
    let m = "func f():\n\tif x == |";
    let ctx = at(m);
    assert_ne!(
        ctx.kind,
        CompletionKind::Assign,
        "`==` is a comparison, not an assignment"
    );
}

#[test]
fn compound_assign_is_assign() {
    // `x += ` → assignment RHS (compound operator).
    let m = "func f():\n\tx += |";
    let ctx = at(m);
    assert_eq!(ctx.kind, CompletionKind::Assign);
}

#[test]
fn chained_attribute_recovers_member_access() {
    // `a.b.c.` → still member access (on the `a.b.c` base subscript).
    let m = "func f():\n\ta.b.c.|";
    let ctx = at(m);
    assert!(
        matches!(ctx.kind, CompletionKind::Attribute { .. }),
        "deep member chain should be Attribute, got {:?}",
        ctx.kind
    );
}

/// A bare `@` (the lexer also emits a co-located `Error` "expected identifier" token) still
/// classifies as an annotation-name context — the `Error` diagnostic marker must not steal the
/// anchor from the `Annotation` token (M8 Phase 4 regression).
#[test]
fn bare_at_is_annotation_despite_error_token() {
    assert_eq!(at("@|").kind, CompletionKind::Annotation);
    assert_eq!(at("@|\n").kind, CompletionKind::Annotation);
    assert_eq!(at("extends Node\n@|\n").kind, CompletionKind::Annotation);
}

// --- #126: string-form node-path prefix span must match the committed-dir split ---

/// CORRUPTION GUARD (#126): in a string node path, `%` is a boundary ONLY as the leading `%Name`
/// root sigil — never mid-path (it can't legally appear mid-segment). The committed dir is split on
/// `/` only (`string_node_path_committed_dir`), so for `get_node("Foo/Bar%Baz|")` the committed dir
/// is `Foo` and the prefix span must be the WHOLE last segment `Bar%Baz`. Accepting a child of `Foo`
/// must then yield `Foo/<child>`, never `Foo/Bar%<child>` (a stale `Bar%` fragment). Reproduce-first:
/// the pre-fix `string_arg_prefix` split the segment on the mid-path `%` and captured only `Baz`.
#[test]
fn string_node_path_mid_percent_splices_whole_segment_no_stale_fragment() {
    let marked = "func f():\n\tget_node(\"Foo/Bar%Baz|\")";
    assert_eq!(
        splice(marked, "Area"),
        "func f():\n\tget_node(\"Foo/Area\")",
        "accepting a child must replace the whole `Bar%Baz` segment, not just `Baz`"
    );
}

/// Regression guard: a `%Name`-rooted string keeps the leading `%` as the boundary — accepting a
/// unique name replaces only the typed name after the `%` (`get_node("%Ui|")` → `get_node("%Health")`).
#[test]
fn string_node_path_rooted_percent_keeps_root_sigil() {
    let marked = "func f():\n\tget_node(\"%Ui|\")";
    assert_eq!(
        splice(marked, "Health"),
        "func f():\n\tget_node(\"%Health\")",
        "a `%`-rooted string replaces only the name after the `%`"
    );
}

/// Regression guard: an ordinary `/`-delimited string path is unaffected — the last segment after
/// the final `/` is replaced (`get_node("A/B/Sp|")` → `get_node("A/B/Sprite")`).
#[test]
fn string_node_path_plain_slash_segment_unchanged() {
    let marked = "func f():\n\tget_node(\"A/B/Sp|\")";
    assert_eq!(
        splice(marked, "Sprite"),
        "func f():\n\tget_node(\"A/B/Sprite\")",
        "the last `/`-delimited segment is replaced"
    );
}

// ===================================================================================================
// Declaration names offer nothing (Godot's `COMPLETION_DECLARATION`, 4.7 parser lines 941/1003/
// 1238/1476/1546/1594 — and the same empty result at 4.6, which never opens a context there).
// ===================================================================================================

#[test]
fn bare_declaration_keyword_offers_nothing() {
    for m in [
        "var |",
        "const |",
        "signal |",
        "enum |",
        "class |",
        "class_name |",
        "static var |",
        "func f():\n\tvar |",
        "func f():\n\tconst |",
    ] {
        assert_eq!(at(m).kind, CompletionKind::None, "fixture {m:?}");
    }
}

#[test]
fn partial_declaration_name_offers_nothing() {
    // The user is inventing a name; every identifier in scope is a name already taken.
    for m in [
        "var spe|",
        "const FO|",
        "signal hea|",
        "enum Sta|",
        "class Inn|",
        "class_name Pla|",
        "static var sha|",
        "func f():\n\tvar loc|",
    ] {
        let ctx = at(m);
        assert_eq!(ctx.kind, CompletionKind::None, "fixture {m:?}");
        assert_eq!(ctx.prefix, None, "a suppressed context carries no prefix");
    }
}

#[test]
fn declaration_suppression_stops_at_the_name() {
    // Only the name position is suppressed — the type, the initializer, and `extends` after a
    // `class_name` all keep their own contexts.
    assert_eq!(
        at("func f():\n\tvar speed: Vec|").kind,
        CompletionKind::TypeName
    );
    // At class level the same `:` is Godot's `COMPLETION_PROPERTY_DECLARATION_OR_TYPE`, since the
    // declaration could still turn into a property block.
    assert_eq!(
        at("var speed: Vec|").kind,
        CompletionKind::PropertyDeclarationOrType
    );
    assert_eq!(at("var speed = |").kind, CompletionKind::Assign);
    assert_eq!(
        at("class_name Player extends Nod|").kind,
        CompletionKind::InheritType
    );
    assert_eq!(
        at("class Inner extends Nod|").kind,
        CompletionKind::InheritType
    );
    // `func <name>` is a declaration too, but Godot gives it override-method completion, not
    // `COMPLETION_DECLARATION`. It must not be swept up here.
    assert_eq!(
        at("func _rea|").kind,
        CompletionKind::OverrideMethod { is_static: false }
    );
}

// ===================================================================================================
// A fresh statement, and a cursor sitting inside a word (#404).
// ===================================================================================================

#[test]
fn a_blank_line_opens_an_identifier_position() {
    // The anchor is whatever ended the previous line — a literal, a `)`, an identifier. None of
    // them start an expression, but the line break between them and the cursor does.
    for m in [
        "func f():\n\tvar a := 5\n\t|",
        "func f():\n\tvar a := 5\n\t|\n\tprint(a)",
        "func f():\n\tprint(1)\n\t|",
        "extends Node\n\nvar speed := 1\n|",
        "extends Node\n\nvar speed := 1\n\nfunc f():\n\tpass\n|",
    ] {
        assert_eq!(at(m).kind, CompletionKind::Identifier, "fixture {m:?}");
    }
}

#[test]
fn a_statement_that_opens_with_an_identifier_completes_from_its_first_column() {
    // `\t|speed = 1` and `\tspe|ed = 1`: neither position is glued to the end of a word, and both
    // used to resolve to `None` because the anchor was the previous line's last token.
    let before = "func f():\n\tvar a := 5\n\t|speed = 1";
    assert_eq!(at(before).kind, CompletionKind::Identifier);
    assert_eq!(prefix_text(before, &at(before)), None);

    let inside = "func f():\n\tvar a := 5\n\tspe|ed = 1";
    let ctx = at(inside);
    assert_eq!(ctx.kind, CompletionKind::Identifier);
    assert_eq!(
        prefix_text(inside, &ctx).as_deref(),
        Some("speed"),
        "the whole word is the prefix, so the edit replaces it"
    );
}

#[test]
fn a_cursor_inside_a_word_takes_the_whole_word_as_the_prefix() {
    for (m, want) in [
        ("func f():\n\tvar v := Vec|tor2.ONE", "Vector2"),
        ("func f():\n\tvar v := Vector2.O|NE", "ONE"),
        ("func f():\n\tprint(v.len|gth())", "length"),
        ("func f():\n\tsuper.re|ady()", "ready"),
    ] {
        assert_eq!(
            prefix_text(m, &at(m)).as_deref(),
            Some(want),
            "fixture {m:?}"
        );
    }
}

#[test]
fn a_cursor_inside_a_member_name_stays_a_member_completion() {
    let m = "func f():\n\tvar local = 1\n\tlocal.na|me";
    assert!(
        matches!(at(m).kind, CompletionKind::Attribute { .. }),
        "got {:?}",
        at(m).kind
    );
}

#[test]
fn a_finished_expression_at_the_end_of_a_line_offers_nothing() {
    // No line break between the `)` and the cursor, so nothing opens here.
    for m in [
        "func f():\n\tprint(1)|",
        "func f():\n\tvar a := 5\n\ta.b()|",
    ] {
        assert_eq!(at(m).kind, CompletionKind::None, "fixture {m:?}");
    }
}

#[test]
fn a_half_typed_declaration_keyword_is_still_a_word_prefix() {
    // `va|r` and `var|` are the user typing the keyword; `var |` is the name position.
    for (m, want) in [
        ("func f():\n\tva|r", Some("var")),
        ("func f():\n\tvar|", Some("var")),
    ] {
        let ctx = at(m);
        assert_eq!(ctx.kind, CompletionKind::Identifier, "fixture {m:?}");
        assert_eq!(prefix_text(m, &ctx).as_deref(), want, "fixture {m:?}");
    }
    assert_eq!(at("func f():\n\tvar |").kind, CompletionKind::None);
}

/// #511: a `static func` cursor is an override position too. Godot opens
/// `COMPLETION_OVERRIDE_METHOD` from `parse_function` regardless of staticness
/// (`gdscript_parser.cpp:1781`) and reads `is_static` back off the node it opened on
/// (`gdscript_editor.cpp:3688`) to filter both halves of the list. Before this, `static` broke the
/// "raw predecessor is layout" test, so the classifier fell through to the general identifier set.
#[test]
fn static_func_is_an_override_position() {
    for m in [
        "static func |",
        "static func _|",
        "static func _stat|",
        "class I:\n\tstatic func _|",
    ] {
        assert_eq!(
            at(m).kind,
            CompletionKind::OverrideMethod { is_static: true },
            "`{m}` must classify as a static override position"
        );
    }
    // The prefix is still what accepting an item replaces.
    let m = "static func _stat|";
    assert_eq!(prefix_text(m, &at(m)).as_deref(), Some("_stat"));
}

/// `static` only makes the `func` static when `static` itself opens the line. A `static func`
/// LAMBDA in expression position is still a lambda, and the author is naming it.
#[test]
fn a_static_func_lambda_is_not_an_override_position() {
    for m in [
        "func f():\n\tvar g = static func _|",
        "func f():\n\tvar g = static func na|",
    ] {
        assert!(
            !matches!(at(m).kind, CompletionKind::OverrideMethod { .. }),
            "`{m}` is a named lambda, not an override position"
        );
    }
}

// ---------------------------------------------------------------------------------------------------
// #514 — the type positions Godot opens BEFORE it consumes the name, so an empty slot is a type slot.
// ---------------------------------------------------------------------------------------------------

/// A parameter's type colon. `is_declaration_colon` scans left for a `var`/`const` and bails on the
/// `(` it would have to cross, so every one of these fell through to the general identifier set.
/// Godot reaches the same `parse_type` from `parse_parameter` (`gdscript_parser.cpp:1529`) for all
/// four shapes.
#[test]
fn a_parameter_type_colon_is_a_type_position() {
    for m in [
        "func f(p: |):\n\tpass",
        // Glued to the colon — the user has typed nothing yet either way.
        "func f(p:|):\n\tpass",
        // Not just the first parameter.
        "func f(a: int, q: |):\n\tpass",
        // A bare lambda, a named lambda, and a signal all route through `parse_parameter` too.
        "func f():\n\tvar g = func(p: |): pass",
        "func f():\n\tvar g = func named(p: |): pass",
        "signal s(p: |)",
    ] {
        assert_eq!(
            at(m).kind,
            CompletionKind::TypeName,
            "`{m}` is a parameter type position"
        );
    }
}

/// The second slot of a typed collection. Godot recurses into `parse_type` for it
/// (`gdscript_parser.cpp:3893`), so it is the same context the `[` slot already is.
#[test]
fn a_typed_collection_comma_is_a_type_position() {
    for m in [
        "var a: Dictionary[String, |]",
        "func g(p: Dictionary[String, |]):\n\tpass",
        // Unclosed at end of input — the same slot, still being typed.
        "var a: Dictionary[String, |",
    ] {
        assert_eq!(
            at(m).kind,
            CompletionKind::TypeName,
            "`{m}` is a typed-collection type slot"
        );
    }
}

/// `is` / `is not` / `as` with the cursor past the keyword. Both operators read their right side
/// with `parse_type` (`gdscript_parser.cpp:3826` and `:3465`), which opens the context before it
/// consumes the identifier (`:3871`). `is ` was the worst of the three: it matched no arm at all, so
/// the editor got an empty popup.
#[test]
fn a_type_test_or_cast_keyword_opens_a_type_position() {
    for m in [
        "func f(x):\n\tif x is |:\n\t\tpass",
        "func f(x):\n\tif x is not |:\n\t\tpass",
        "func f(x):\n\tprint(x as |)",
        "func f(x):\n\tvar y := x as |",
    ] {
        assert_eq!(
            at(m).kind,
            CompletionKind::TypeName,
            "`{m}` is a type position"
        );
    }
    // Mid-word, the whole word is the prefix so accepting an item replaces it.
    let m = "func f(x):\n\tprint(x as S|tr)";
    let ctx = at(m);
    assert_eq!(ctx.kind, CompletionKind::TypeName);
    assert_eq!(prefix_text(m, &ctx).as_deref(), Some("Str"));
}

/// The scoping half of #514. Each of these looks like one of the three new arms in the token stream
/// and is not one.
#[test]
fn the_new_type_positions_do_not_swallow_their_look_alikes() {
    for m in [
        // A dictionary entry's colon, at class scope and inside a call.
        "var d := {\"a\": |}",
        "func f():\n\tprint({\"p\": |})",
        // A lambda's BLOCK colon inside a parameter list — the token before it is `)`, not a name.
        "func f(cb = func(): |):\n\tpass",
        // A plain call is not a parameter list.
        "func f():\n\tfoo(a: |)",
        // Commas that are not typed-collection slots.
        "func f():\n\tvar a := [1, |]",
        "func f(d: Dictionary):\n\tprint(d[1, |])",
        "func f(p: int = [1, |]):\n\tpass",
        // The slot for a new parameter NAME, not its type.
        "func f(p, |):\n\tpass",
        // `not` is a type position only as the `is not` pair.
        "func f(x):\n\tif not |:\n\t\tpass",
        "func f(x):\n\tassert(not |)",
    ] {
        assert_ne!(
            at(m).kind,
            CompletionKind::TypeName,
            "`{m}` must not become a type position"
        );
    }
}

/// The positions that already worked keep their exact classification — this widens what is claimed,
/// it does not re-route anything.
#[test]
fn the_type_positions_that_already_worked_are_unchanged() {
    assert_eq!(
        at("var a: |").kind,
        CompletionKind::PropertyDeclarationOrType
    );
    assert_eq!(
        at("func f() -> |:\n\tpass").kind,
        CompletionKind::TypeNameOrVoid
    );
    assert_eq!(at("var a: Array[|]").kind, CompletionKind::TypeName);
    assert_eq!(
        at("func f():\n\tvar y: Array[|] = []").kind,
        CompletionKind::TypeName
    );
}

// ===================================================================================================
// The prose guard (#599) — Godot offers nothing inside a string literal or a comment. The shapes
// that genuinely want completion inside a string are claimed by the deferred pass before the
// guard, and everything that already worked keeps working.
// ===================================================================================================

#[test]
fn no_completion_inside_a_string_literal() {
    // Mid-typing a message string: the whole identifier list used to pop here.
    assert_eq!(
        at("func f():\n\tvar s := \"hello wo|\"\n").kind,
        CompletionKind::None
    );
}

#[test]
fn no_completion_inside_an_empty_string() {
    // `"` is an advertised trigger character, so this fires the moment a string opens.
    assert_eq!(
        at("func f():\n\tvar s := \"|\"\n").kind,
        CompletionKind::None
    );
}

#[test]
fn no_completion_inside_a_line_comment() {
    assert_eq!(
        at("func f():\n\t# a comment her|e\n\tpass\n").kind,
        CompletionKind::None
    );
}

#[test]
fn no_completion_inside_a_doc_comment() {
    assert_eq!(
        at("## doc comment her|e\nfunc f():\n\tpass\n").kind,
        CompletionKind::None
    );
}

#[test]
fn no_completion_at_the_end_of_a_comment_line() {
    // The common typing position: the caret sits after the last comment character, at the
    // comment span's exclusive end.
    assert_eq!(
        at("func f():\n\t# a comment here|\n\tpass\n").kind,
        CompletionKind::None
    );
}

#[test]
fn no_completion_inside_stringname_and_nodepath_literals() {
    // `&"…"` and `^"…"` are Literal tokens too — plain prose, not deferred paths.
    assert_eq!(
        at("func f():\n\tvar a := &\"na|\"\n").kind,
        CompletionKind::None
    );
    assert_eq!(
        at("func f():\n\tvar b := ^\"pa|\"\n").kind,
        CompletionKind::None
    );
}

#[test]
fn no_completion_inside_a_dict_key_string() {
    assert_eq!(
        at("func f():\n\tvar d := {\"ke|\": 1}\n").kind,
        CompletionKind::None
    );
}

#[test]
fn deferred_shapes_still_classify_inside_strings() {
    // Resource paths and node paths are claimed by the deferred pass BEFORE the guard.
    assert_eq!(
        at("func f():\n\tpreload(\"res://src/|\")").kind,
        CompletionKind::Deferred(DeferredReason::ResourcePath)
    );
    assert_eq!(
        at("func f():\n\tload(\"res://|\")").kind,
        CompletionKind::Deferred(DeferredReason::ResourcePath)
    );
    assert_eq!(
        at("func f():\n\tget_node(\"Sp|\")").kind,
        CompletionKind::Deferred(DeferredReason::NodePath)
    );
    assert_eq!(
        at("func f():\n\t$\"He|\"").kind,
        CompletionKind::Deferred(DeferredReason::NodePath)
    );
}

#[test]
fn member_access_after_a_string_still_classifies() {
    // `"abc".` — the cursor is past the literal, never strictly inside it.
    let m = "func f():\n\tvar n := \"abc\".|\n";
    assert!(matches!(at(m).kind, CompletionKind::Attribute { .. }));
}
