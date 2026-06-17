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
    let tree = gd_syntax::parse(&src).tree;
    let (tokens, _errs) = tokenize(&src);
    classify(&tree, &tokens, byte)
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
    assert_eq!(ctx.kind, CompletionKind::OverrideMethod);
    assert_eq!(prefix_text(m, &ctx).as_deref(), Some("_re"));
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
        "var x: int:\n\tg|",
        "var x: int:\n\ts|",
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
        CompletionKind::OverrideMethod,
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
    let tree = gd_syntax::parse(src).tree;
    let (tokens, _e) = tokenize(src);
    let ctx = classify(&tree, &tokens, src.len());
    let CompletionKind::Attribute { base: Some(base) } = ctx.kind else {
        panic!("expected Attribute with a base, got {:?}", ctx.kind);
    };
    match &tree.get(base).kind {
        NodeKind::Identifier(id) => assert_eq!(id.name, "local"),
        other => panic!("base should be the `local` Identifier, got {other:?}"),
    }
}

#[test]
fn call_arguments_callee_node_recovered_when_ast_survives() {
    // `max(1, , 2)` keeps a Call node, so the callee node id is recoverable (an Identifier "max").
    let src = "func f():\n\tmax(1, , 2)";
    let cursor = src.find(", , ").unwrap() + 2; // empty middle slot
    let tree = gd_syntax::parse(src).tree;
    let (tokens, _e) = tokenize(src);
    let ctx = classify(&tree, &tokens, cursor);
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
    match &tree.get(callee).kind {
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
            format!("func f():\n\tload(\"res://src/foo.gd\")"),
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
        let tree = gd_syntax::parse(src).tree;
        let (tokens, _errs) = tokenize(src);
        // Inclusive of 0 and len, plus one past, plus a wildly-out-of-range offset — all must clamp
        // to a well-defined result rather than panic.
        for byte in 0..=src.len() + 1 {
            let _ = classify(&tree, &tokens, byte);
        }
        let _ = classify(&tree, &tokens, usize::MAX);
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
            let tree = gd_syntax::parse(&src).tree;
            let (tokens, _errs) = tokenize(&src);
            // Step a few bytes at a time to keep this fast over the whole corpus.
            let mut byte = 0;
            while byte <= src.len() {
                let _ = classify(&tree, &tokens, byte);
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
