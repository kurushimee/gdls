//! Per-code emission tests for the warning sites ported in #29 — the codes that were declared
//! (enum/template/policy) but had no emission site through v1.0.2. Each test pins one upstream
//! site's condition and anchor against gdscript_analyzer.cpp / gdscript_parser.cpp @ 4.6.3-stable.
//! The vendored corpus is upstream-only (no local fixtures), so these live here as unit tests.

use gd_analyze::warn_policy::{StrictSettings, WarnPolicy};
use gd_analyze::warnings::WarningCode;
use gd_analyze::NoCrossFile;
use gd_project::WarningConfig;
use gd_types::NativeDb;

fn mini_native() -> NativeDb {
    NativeDb::from_json(
        r#"{
            "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
            "classes": [
                {"name": "Object"},
                {"name": "Node", "inherits": "Object"}
            ]
        }"#,
    )
    .expect("valid mini dump")
}

fn godot_policy() -> WarnPolicy {
    WarnPolicy::build(&WarningConfig::default(), &StrictSettings::default())
}

/// Default policy plus `enable_warnings` for ignore-by-default codes under test.
fn policy_enabling(names: &[&str]) -> WarnPolicy {
    let strict = StrictSettings {
        enable_warnings: names.iter().map(|s| s.to_string()).collect(),
        ..Default::default()
    };
    WarnPolicy::build(&WarningConfig::default(), &strict)
}

/// Analyze `src` and return each warning as `(code, 1-based line of the diagnostic span start)`.
fn warnings_with_lines(src: &str, policy: &WarnPolicy) -> Vec<(WarningCode, u32)> {
    let tree = gd_syntax::parse(src).tree;
    let native = mini_native();
    let result = gd_analyze::analyze(&tree, None, "t.gd", &native, &NoCrossFile, policy);
    result
        .diagnostics
        .iter()
        .filter_map(|d| {
            let code = d.warning_code()?;
            let line = 1 + src.as_bytes()[..d.span().start.min(src.len())]
                .iter()
                .filter(|&&b| b == b'\n')
                .count() as u32;
            Some((code, line))
        })
        .collect()
}

fn codes(src: &str, policy: &WarnPolicy) -> Vec<WarningCode> {
    warnings_with_lines(src, policy)
        .into_iter()
        .map(|(c, _)| c)
        .collect()
}

// --- EMPTY_FILE (gdscript_parser.cpp:482-489) -------------------------------------------------

#[test]
fn empty_file_warns_on_comment_only_source() {
    for src in ["", "\n\n", "# just a comment\n", "  \n# c\n\n"] {
        assert_eq!(
            codes(src, &godot_policy()),
            vec![WarningCode::EmptyFile],
            "source {src:?} holds no tokens — Godot warns EMPTY_FILE"
        );
    }
}

#[test]
fn empty_file_silent_on_any_real_token() {
    for src in ["extends Node\n", "var x = 1\n", "pass"] {
        assert!(
            !codes(src, &godot_policy()).contains(&WarningCode::EmptyFile),
            "source {src:?} has tokens — no EMPTY_FILE"
        );
    }
}

// --- STANDALONE_EXPRESSION / STANDALONE_TERNARY / RETURN_VALUE_DISCARDED("preload")
//     (gdscript_parser.cpp:2132-2160) ----------------------------------------------------------

#[test]
fn standalone_expression_on_effectless_statement() {
    let src = "extends Node\n\n\nfunc f() -> void:\n\t1 + 2\n";
    assert_eq!(
        warnings_with_lines(src, &godot_policy()),
        vec![(WarningCode::StandaloneExpression, 5)]
    );
}

#[test]
fn standalone_expression_on_bare_identifier_and_literal() {
    // Default arm (identifier) + the literal arm (non-String literal).
    let src = "extends Node\n\nvar x = 1\n\n\nfunc f() -> void:\n\tx\n\t3.5\n\t&\"sname\"\n";
    let got = warnings_with_lines(src, &godot_policy());
    assert_eq!(
        got,
        vec![
            (WarningCode::StandaloneExpression, 7),
            (WarningCode::StandaloneExpression, 8),
            (WarningCode::StandaloneExpression, 9),
        ]
    );
}

#[test]
fn standalone_expression_exempts_string_literal_and_effectful_kinds() {
    // A String literal doubles as a multiline comment (Godot exempts Variant::STRING only);
    // assignments, awaits, and calls are effectful.
    let src = "extends Node\n\nvar x = 1\n\n\nfunc f() -> void:\n\t\"doc comment\"\n\tx = 2\n\tprint(x)\n";
    assert!(!codes(src, &godot_policy()).contains(&WarningCode::StandaloneExpression));
}

#[test]
fn standalone_ternary_on_ternary_statement() {
    let src = "extends Node\n\n\nfunc f(a: int) -> void:\n\t1 if a > 0 else 2\n";
    assert_eq!(
        warnings_with_lines(src, &godot_policy()),
        vec![(WarningCode::StandaloneTernary, 5)]
    );
}

#[test]
fn preload_statement_discards_return_value() {
    // RETURN_VALUE_DISCARDED is ignore-by-default (Godot's default level) — enable it.
    let src = "extends Node\n\n\nfunc f() -> void:\n\tpreload(\"res://icon.svg\")\n";
    let policy = policy_enabling(&["RETURN_VALUE_DISCARDED"]);
    assert_eq!(
        warnings_with_lines(src, &policy),
        vec![(WarningCode::ReturnValueDiscarded, 5)]
    );
    // ...and stays silent at defaults.
    assert_eq!(codes(src, &godot_policy()), vec![]);
}

// --- UNREACHABLE_CODE (gdscript_parser.cpp:2005, 2205-2215) -----------------------------------

#[test]
fn unreachable_code_after_return_warns_once() {
    // Only the FIRST unreachable statement is flagged (per-suite latch), and the standalone
    // shape warning on the same statement precedes it (parse-queue order).
    let src = "extends Node\n\n\nfunc f() -> void:\n\treturn\n\t1 + 2\n\t2 + 3\n";
    assert_eq!(
        warnings_with_lines(src, &godot_policy()),
        vec![
            (WarningCode::StandaloneExpression, 6),
            (WarningCode::UnreachableCode, 6),
            (WarningCode::StandaloneExpression, 7),
        ]
    );
    let msg_src = "extends Node\n\n\nfunc f() -> void:\n\treturn\n\tpass\n";
    let tree = gd_syntax::parse(msg_src).tree;
    let r = gd_analyze::analyze(
        &tree,
        None,
        "t.gd",
        &mini_native(),
        &NoCrossFile,
        &godot_policy(),
    );
    let msg = r
        .diagnostics
        .iter()
        .find(|d| d.warning_code() == Some(WarningCode::UnreachableCode))
        .expect("unreachable fires")
        .message()
        .to_owned();
    assert_eq!(
        msg,
        r#"Unreachable code (statement after return) in function "f()"."#
    );
}

#[test]
fn unreachable_code_after_exhaustive_if_else() {
    let src = "extends Node\n\n\nfunc f(a: int) -> void:\n\tif a > 0:\n\t\treturn\n\telse:\n\t\treturn\n\tpass\n";
    assert_eq!(
        warnings_with_lines(src, &godot_policy()),
        vec![(WarningCode::UnreachableCode, 9)]
    );
}

#[test]
fn unreachable_code_after_exhaustive_match() {
    // Every branch returns AND a wildcard exists (gdscript_parser.cpp:2458-2460).
    let src = "extends Node\n\n\nfunc f(v: int) -> void:\n\tmatch v:\n\t\t1:\n\t\t\treturn\n\t\t_:\n\t\t\treturn\n\tpass\n";
    assert_eq!(
        warnings_with_lines(src, &godot_policy()),
        vec![(WarningCode::UnreachableCode, 10)]
    );
}

#[test]
fn no_unreachable_after_nonexhaustive_constructs() {
    // if without else; match without wildcard; while — none guarantee a return.
    let src = "extends Node\n\n\nfunc f(a: int) -> void:\n\tif a > 0:\n\t\treturn\n\tmatch a:\n\t\t1:\n\t\t\treturn\n\twhile a > 0:\n\t\treturn\n\tpass\n";
    assert!(!codes(src, &godot_policy()).contains(&WarningCode::UnreachableCode));
}

#[test]
fn unreachable_code_in_anonymous_lambda() {
    let src = "extends Node\n\n\nfunc f() -> void:\n\tvar g = func():\n\t\treturn\n\t\tpass\n\tg.call()\n";
    let tree = gd_syntax::parse(src).tree;
    let r = gd_analyze::analyze(
        &tree,
        None,
        "t.gd",
        &mini_native(),
        &NoCrossFile,
        &godot_policy(),
    );
    let msg = r
        .diagnostics
        .iter()
        .find(|d| d.warning_code() == Some(WarningCode::UnreachableCode))
        .expect("unreachable fires inside the lambda body")
        .message()
        .to_owned();
    assert_eq!(
        msg,
        r#"Unreachable code (statement after return) in function "<anonymous lambda>()"."#
    );
}

// --- UNREACHABLE_PATTERN (gdscript_parser.cpp:2433-2436) --------------------------------------

#[test]
fn unreachable_pattern_after_wildcard() {
    let src = "extends Node\n\n\nfunc f(v: int) -> void:\n\tmatch v:\n\t\t_:\n\t\t\tpass\n\t\t1:\n\t\t\tpass\n";
    assert_eq!(
        warnings_with_lines(src, &godot_policy()),
        vec![(WarningCode::UnreachablePattern, 8)]
    );
}

#[test]
fn unreachable_pattern_after_bind_all() {
    // A bare bind pattern (`var x:`) also sets has_wildcard.
    let src = "extends Node\n\n\nfunc f(v: int) -> void:\n\tmatch v:\n\t\tvar x:\n\t\t\tprint(x)\n\t\t1:\n\t\t\tpass\n";
    assert_eq!(
        warnings_with_lines(src, &godot_policy()),
        vec![(WarningCode::UnreachablePattern, 8)]
    );
}

#[test]
fn no_unreachable_pattern_before_wildcard() {
    let src = "extends Node\n\n\nfunc f(v: int) -> void:\n\tmatch v:\n\t\t1:\n\t\t\tpass\n\t\t_:\n\t\t\tpass\n";
    assert!(!codes(src, &godot_policy()).contains(&WarningCode::UnreachablePattern));
}

// --- ASSERT_ALWAYS_TRUE / ASSERT_ALWAYS_FALSE (analyzer.cpp:2393-2399) ------------------------

#[test]
fn assert_always_true_on_constant_truthy_condition() {
    for cond in ["true", "1 == 1", "42"] {
        let src = format!("extends Node\n\n\nfunc f() -> void:\n\tassert({cond})\n");
        assert_eq!(
            warnings_with_lines(&src, &godot_policy()),
            vec![(WarningCode::AssertAlwaysTrue, 5)],
            "assert({cond})"
        );
    }
}

#[test]
fn assert_always_false_on_constant_falsy_condition_except_bool_literal() {
    let src = "extends Node\n\n\nfunc f() -> void:\n\tassert(1 == 2)\n";
    assert_eq!(
        warnings_with_lines(src, &godot_policy()),
        vec![(WarningCode::AssertAlwaysFalse, 5)]
    );
    // `assert(false)` is a deliberate trap — Godot exempts the bool literal.
    let src = "extends Node\n\n\nfunc f() -> void:\n\tassert(false)\n";
    assert_eq!(codes(src, &godot_policy()), vec![]);
}

#[test]
fn assert_on_runtime_condition_is_silent() {
    let src = "extends Node\n\n\nfunc f(a: int) -> void:\n\tassert(a > 0)\n";
    assert_eq!(codes(src, &godot_policy()), vec![]);
}

// --- INTEGER_DIVISION (analyzer.cpp:3104-3113) ------------------------------------------------

#[test]
fn integer_division_on_int_operands() {
    // The issue #29 repro: a constant int division in an initializer. (`_y` is also an unused
    // private member — that pre-existing warning rides along.)
    let src = "extends Node\n\nvar _y = 10 / 3\n";
    assert_eq!(
        warnings_with_lines(src, &godot_policy()),
        vec![
            (WarningCode::IntegerDivision, 3),
            (WarningCode::UnusedPrivateClassVariable, 3),
        ]
    );
    // Locals too, via the function body path.
    let src = "extends Node\n\n\nfunc f(a: int, b: int) -> void:\n\tvar _q = a / b\n";
    assert_eq!(
        warnings_with_lines(src, &godot_policy()),
        vec![(WarningCode::IntegerDivision, 5)]
    );
}

#[test]
fn integer_division_silent_when_either_side_is_float() {
    for expr in ["10.0 / 3", "10 / 3.0", "10.0 / 3.0"] {
        let src = format!("extends Node\n\nvar _y = {expr}\n");
        assert!(
            !codes(&src, &godot_policy()).contains(&WarningCode::IntegerDivision),
            "{expr}"
        );
    }
}

// --- INCOMPATIBLE_TERNARY (analyzer.cpp:5172-5184) --------------------------------------------

#[test]
fn incompatible_ternary_when_neither_branch_accepts_the_other() {
    let src = "extends Node\n\n\nfunc f(a: int) -> void:\n\tvar _x = 1 if a > 0 else \"s\"\n";
    assert_eq!(
        warnings_with_lines(src, &godot_policy()),
        vec![(WarningCode::IncompatibleTernary, 5)]
    );
}

#[test]
fn compatible_or_variant_ternary_is_silent() {
    // Same type — fine; a Variant branch — exempt (upstream's is_variant() arm).
    for tail in ["1 if a > 0 else 2", "v if a > 0 else 2"] {
        let src =
            format!("extends Node\n\nvar v\n\n\nfunc f(a: int) -> void:\n\tvar _x = {tail}\n");
        assert!(
            !codes(&src, &godot_policy()).contains(&WarningCode::IncompatibleTernary),
            "{tail}"
        );
    }
}

// --- UNTYPED_DECLARATION / INFERRED_DECLARATION
//     (analyzer.cpp:2176-2191, 1131-1135, 1801-1820, 1966-1969, 2356-2362) --------------------

#[test]
fn untyped_declaration_fires_across_declaration_kinds() {
    let policy = policy_enabling(&["UNTYPED_DECLARATION"]);
    // Variable (member + local), parameter, signal parameter, function return.
    let src = "extends Node\n\nvar m = 1\nsignal hit(amount)\n\n\nfunc f(p) -> void:\n\tvar l = p\n\tprint(l)\n\n\nfunc g() :\n\treturn\n";
    let got = warnings_with_lines(src, &policy);
    let untyped: Vec<u32> = got
        .iter()
        .filter(|(c, _)| *c == WarningCode::UntypedDeclaration)
        .map(|&(_, l)| l)
        .collect();
    assert_eq!(
        untyped,
        vec![3, 4, 7, 8, 12],
        "member var, signal param, func param, local var, untyped-return function; got {got:?}"
    );
}

#[test]
fn untyped_declaration_message_shapes() {
    let policy = policy_enabling(&["UNTYPED_DECLARATION"]);
    let src = "extends Node\n\n\nfunc g():\n\treturn\n";
    let tree = gd_syntax::parse(src).tree;
    let r = gd_analyze::analyze(&tree, None, "t.gd", &mini_native(), &NoCrossFile, &policy);
    let msgs: Vec<String> = r
        .diagnostics
        .iter()
        .filter(|d| d.warning_code() == Some(WarningCode::UntypedDeclaration))
        .map(|d| d.message().to_owned())
        .collect();
    assert_eq!(msgs, vec![r#"Function "g()" has no static return type."#]);
}

#[test]
fn inferred_declaration_on_walrus_and_const() {
    let policy = policy_enabling(&["INFERRED_DECLARATION"]);
    let src = "extends Node\n\nvar a := 1\nconst B = 2\n";
    let got: Vec<u32> = warnings_with_lines(src, &policy)
        .into_iter()
        .filter(|(c, _)| *c == WarningCode::InferredDeclaration)
        .map(|(_, l)| l)
        .collect();
    assert_eq!(got, vec![3, 4]);
}

#[test]
fn inferred_declaration_exempts_constant_type_import() {
    // `const V = Vector2` re-exports a type — no way to spell its true type (analyzer.cpp:2181).
    let policy = policy_enabling(&["INFERRED_DECLARATION"]);
    let src = "extends Node\n\nconst N = Node\n";
    assert!(
        !codes(src, &policy).contains(&WarningCode::InferredDeclaration),
        "type imports are exempt"
    );
}

#[test]
fn for_iterator_inferred_vs_untyped() {
    let policy = policy_enabling(&["UNTYPED_DECLARATION", "INFERRED_DECLARATION"]);
    // `range()` infers a hard int — INFERRED; an untyped Array list leaves it soft — UNTYPED.
    let src = "extends Node\n\n\nfunc f(arr: Array) -> void:\n\tfor i in range(3):\n\t\tprint(i)\n\tfor v in arr:\n\t\tprint(v)\n";
    let got = warnings_with_lines(src, &policy);
    assert!(
        got.contains(&(WarningCode::InferredDeclaration, 5)),
        "range() iterator is hard-typed => inferred; got {got:?}"
    );
    assert!(
        got.contains(&(WarningCode::UntypedDeclaration, 7)),
        "untyped-Array iterator is soft => untyped; got {got:?}"
    );
}

#[test]
fn typed_declarations_are_silent() {
    let policy = policy_enabling(&["UNTYPED_DECLARATION", "INFERRED_DECLARATION"]);
    let src = "extends Node\n\nvar a: int = 1\nconst B: int = 2\nsignal hit(amount: int)\n\n\nfunc f(p: int) -> void:\n\tvar l: int = p\n\tfor i: int in [1]:\n\t\tprint(l + i)\n";
    let got = warnings_with_lines(src, &policy);
    assert!(
        !got.iter().any(|(c, _)| matches!(
            c,
            WarningCode::UntypedDeclaration | WarningCode::InferredDeclaration
        )),
        "fully annotated declarations must not warn; got {got:?}"
    );
}

// --- untyped rest parameter (analyzer.cpp:1801-1820): inferred Array, not an error -------------

#[test]
fn untyped_rest_parameter_is_inferred_array_not_an_error() {
    // Regression: `func f(...args):` false-positived `The rest parameter type must be "Array",
    // but "Variant" is specified.` through v1.0.2.
    let src = "extends Node\n\n\nfunc f(...args) -> void:\n\tprint(args)\n";
    let tree = gd_syntax::parse(src).tree;
    let r = gd_analyze::analyze(
        &tree,
        None,
        "t.gd",
        &mini_native(),
        &NoCrossFile,
        &godot_policy(),
    );
    let errors: Vec<&str> = r
        .diagnostics
        .iter()
        .filter(|d| d.severity() == gd_analyze::Severity::Error)
        .map(|d| d.message())
        .collect();
    assert_eq!(errors, Vec::<&str>::new());

    // With the warning enabled, the untyped rest param warns twice — the generic
    // resolve_assignable site plus the dedicated vararg site, exactly as upstream.
    let policy = policy_enabling(&["UNTYPED_DECLARATION"]);
    let n = warnings_with_lines(
        "extends Node\n\n\nfunc f(...args) -> void:\n\tprint(args)\n",
        &policy,
    )
    .into_iter()
    .filter(|&(c, l)| c == WarningCode::UntypedDeclaration && l == 4)
    .count();
    assert_eq!(n, 2, "generic + dedicated vararg sites, as upstream");

    // A typed rest param still validates: non-Array errors.
    let src = "extends Node\n\n\nfunc f(...args: int) -> void:\n\tprint(args)\n";
    let tree = gd_syntax::parse(src).tree;
    let r = gd_analyze::analyze(
        &tree,
        None,
        "t.gd",
        &mini_native(),
        &NoCrossFile,
        &godot_policy(),
    );
    assert!(r.diagnostics.iter().any(|d| d
        .message()
        .contains(r#"The rest parameter type must be "Array""#)));
}

// --- REDUNDANT_STATIC_UNLOAD (analyzer.cpp:1275, 1318-1338) ------------------------------------

#[test]
fn redundant_static_unload_without_static_data() {
    let src = "@static_unload\nextends Node\n\nvar x = 1\n";
    assert_eq!(
        warnings_with_lines(src, &godot_policy()),
        vec![(WarningCode::RedundantStaticUnload, 1)]
    );
}

#[test]
fn static_unload_with_static_data_is_silent() {
    for member in [
        "static var s = 1",
        "static func _static_init() -> void:\n\tpass",
    ] {
        let src = format!("@static_unload\nextends Node\n\n{member}\n");
        assert!(
            !codes(&src, &godot_policy()).contains(&WarningCode::RedundantStaticUnload),
            "{member}"
        );
    }
    // A direct inner class's own static data also counts (upstream's member.m_class flag read).
    let src = "@static_unload\nextends Node\n\nclass Inner:\n\tstatic var s = 1\n";
    assert!(!codes(src, &godot_policy()).contains(&WarningCode::RedundantStaticUnload));
}

// --- UNASSIGNED_VARIABLE / UNASSIGNED_VARIABLE_OP_ASSIGN
//     (analyzer.cpp:4435-4439, 2852-2860, 3043-3050) -------------------------------------------

#[test]
fn unassigned_variable_flow_insensitive_counter() {
    // The upstream parser/warnings/unassigned_variable.gd shape: reads before the first
    // assignment warn; once any assignment was traversed, later reads stay silent
    // ("maybe assigned"), even when the assignment sat in a dead branch.
    let src = "extends Node\n\n\nfunc test() -> void:\n\tvar unassigned\n\tprint(unassigned)\n\tunassigned = \"something\"\n\n\tvar a\n\tprint(a)\n\tif a:\n\t\ta = 1\n\t\tprint(a)\n\tprint(a)\n";
    let got: Vec<u32> = warnings_with_lines(src, &godot_policy())
        .into_iter()
        .filter(|(c, _)| *c == WarningCode::UnassignedVariable)
        .map(|(_, l)| l)
        .collect();
    assert_eq!(got, vec![6, 10, 11], "print(unassigned); print(a); if a:");
}

#[test]
fn unassigned_variable_exempts_hard_builtin_and_initialized() {
    // `var x: int` zero-initializes meaningfully (hard builtin) — exempt; an initializer
    // counts as the first assignment; member reads never fire (source classified post-switch).
    let src = "extends Node\n\nvar m\n\n\nfunc test() -> void:\n\tvar i: int\n\tprint(i)\n\tvar j = 1\n\tprint(j)\n\tprint(m)\n";
    assert!(!codes(src, &godot_policy()).contains(&WarningCode::UnassignedVariable));
}

#[test]
fn unassigned_variable_op_assign_on_uninitialized_local() {
    // upstream parser/warnings/unassigned_variable_op_assign.gd — note `var __: int` is
    // hard-builtin so the plain UNASSIGNED_VARIABLE read check stays quiet, but the
    // compound assignment still warns.
    let src = "extends Node\n\n\nfunc test() -> void:\n\tvar __: int\n\t__ += 15\n";
    let tree = gd_syntax::parse(src).tree;
    let r = gd_analyze::analyze(
        &tree,
        None,
        "t.gd",
        &mini_native(),
        &NoCrossFile,
        &godot_policy(),
    );
    let msgs: Vec<&str> = r
        .diagnostics
        .iter()
        .filter(|d| d.warning_code() == Some(WarningCode::UnassignedVariableOpAssign))
        .map(|d| d.message())
        .collect();
    assert_eq!(
        msgs,
        vec![
            r#"The variable "__" is modified with the compound-assignment operator "+=" but was not previously initialized."#
        ]
    );
}

#[test]
fn op_assign_after_initialization_is_silent() {
    for body in [
        "\tvar x = 1\n\tx += 1\n\tprint(x)\n", // initializer counts
        "\tvar x: int\n\tx = 1\n\tx += 1\n\tprint(x)\n", // plain assignment counts
    ] {
        let src = format!("extends Node\n\n\nfunc test() -> void:\n{body}");
        assert!(
            !codes(&src, &godot_policy()).contains(&WarningCode::UnassignedVariableOpAssign),
            "{body}"
        );
    }
}

// --- UNUSED_VARIABLE / UNUSED_LOCAL_CONSTANT (analyzer.cpp:2214-2218, 2227-2231) ---------------

#[test]
fn unused_variable_and_local_constant_warn() {
    let src =
        "extends Node\n\n\nfunc test() -> void:\n\tvar dead = 1\n\tconst DEAD_C = 2\n\tpass\n";
    let got = warnings_with_lines(src, &godot_policy());
    assert!(
        got.contains(&(WarningCode::UnusedVariable, 5)),
        "got {got:?}"
    );
    assert!(
        got.contains(&(WarningCode::UnusedLocalConstant, 6)),
        "got {got:?}"
    );
}

#[test]
fn used_underscored_or_written_locals_are_silent() {
    // A read, an underscore prefix, or even a write-only assignment (Godot's parser counts
    // assignee identifiers as usages) all suppress.
    let src = "extends Node\n\n\nfunc test() -> void:\n\tvar used = 1\n\tprint(used)\n\tvar _ignored = 2\n\tconst _IGN = 3\n\tvar written = 4\n\twritten = 5\n";
    let got = codes(src, &godot_policy());
    assert!(
        !got.contains(&WarningCode::UnusedVariable)
            && !got.contains(&WarningCode::UnusedLocalConstant),
        "got {got:?}"
    );
}

// --- RETURN_VALUE_DISCARDED (call statements, analyzer.cpp:3684-3689) --------------------------

#[test]
fn return_value_discarded_on_non_void_call_statement() {
    let policy = policy_enabling(&["RETURN_VALUE_DISCARDED"]);
    let src =
        "extends Node\n\n\nfunc gives() -> int:\n\treturn 1\n\n\nfunc f() -> void:\n\tgives()\n";
    assert_eq!(
        warnings_with_lines(src, &policy),
        vec![(WarningCode::ReturnValueDiscarded, 9)]
    );
    // Void calls, used-value calls, and the ignore-by-default level all stay silent.
    let silent = "extends Node\n\n\nfunc gives() -> int:\n\treturn 1\n\n\nfunc nothing() -> void:\n\tpass\n\n\nfunc f() -> void:\n\tnothing()\n\tvar _x = gives()\n";
    assert_eq!(codes(silent, &policy), vec![]);
    assert_eq!(codes(src, &godot_policy()), vec![]);
}

// --- STATIC_CALLED_ON_INSTANCE (analyzer.cpp:3691-3694) ----------------------------------------

#[test]
fn static_called_on_instance_warns() {
    let src = "class_name Helper\nextends Node\n\nstatic func compute() -> int:\n\treturn 1\n\n\nfunc f(h: Helper) -> void:\n\tvar _x = h.compute()\n";
    let tree = gd_syntax::parse(src).tree;
    let r = gd_analyze::analyze(
        &tree,
        None,
        "t.gd",
        &mini_native(),
        &NoCrossFile,
        &godot_policy(),
    );
    let msgs: Vec<&str> = r
        .diagnostics
        .iter()
        .filter(|d| d.warning_code() == Some(WarningCode::StaticCalledOnInstance))
        .map(|d| d.message())
        .collect();
    assert_eq!(
        msgs,
        vec![
            r#"The function "compute()" is a static function but was called from an instance. Instead, it should be directly called from the type: "Helper.compute()"."#
        ]
    );
}

#[test]
fn static_called_through_class_or_self_is_silent() {
    // Through the class name (meta base), or bare/self on the own class — no warning.
    let src = "class_name Helper2\nextends Node\n\nstatic func compute() -> int:\n\treturn 1\n\n\nfunc f() -> void:\n\tvar _a = Helper2.compute()\n\tvar _b = compute()\n";
    assert!(!codes(src, &godot_policy()).contains(&WarningCode::StaticCalledOnInstance));
}

// --- INT_AS_ENUM_WITHOUT_CAST (analyzer.cpp:6139-6150 via node-passing callers) ----------------

#[test]
fn int_as_enum_without_cast_on_initializer_and_assignment() {
    let src = "extends Node\n\nenum State { OFF = 1, ON = 2 }\n\nvar s: State = 1\n\n\nfunc f() -> void:\n\ts = 2\n";
    let got: Vec<u32> = warnings_with_lines(src, &godot_policy())
        .into_iter()
        .filter(|(c, _)| *c == WarningCode::IntAsEnumWithoutCast)
        .map(|(_, l)| l)
        .collect();
    assert_eq!(got, vec![5, 9], "initializer line 5 + assignment line 9");
}

#[test]
fn enum_value_flows_without_cast_warning() {
    // Assigning a real enum member (or a cast int) is the blessed shape.
    let src = "extends Node\n\nenum State { OFF = 1, ON = 2 }\n\nvar s: State = State.OFF\n\n\nfunc f() -> void:\n\ts = State.ON\n\ts = 2 as State\n";
    assert!(!codes(src, &godot_policy()).contains(&WarningCode::IntAsEnumWithoutCast));
}

// --- UNSAFE_PROPERTY_ACCESS (gdscript_analyzer.cpp:4878-4886) — issue #32 ---------------------

/// A member-rich dump for the attribute-walk tests: a native class with a property, signal,
/// enum (+ value), constant, and method, plus a memberless subclass to force inherits-chain
/// walks. The bare `mini_native()` above stays member-free for the older tests' expectations.
fn member_native() -> NativeDb {
    NativeDb::from_json(
        r#"{
            "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
            "classes": [
                {"name": "Object"},
                {"name": "Node", "inherits": "Object",
                 "properties": [{"name": "mode", "type": "int", "setter": "set_mode", "getter": "get_mode"}],
                 "signals": [{"name": "renamed"}],
                 "enums": [{"name": "Kind", "is_bitfield": false, "values": [{"name": "KIND_A", "value": 0}]}],
                 "constants": [{"name": "NOTIF_READY", "value": 13}],
                 "methods": [{"name": "get_name", "is_const": true, "is_static": false, "is_vararg": false,
                              "is_virtual": false, "hash": 1, "return_value": {"type": "String"}, "arguments": []}]},
                {"name": "Node2D", "inherits": "Node"}
            ]
        }"#,
    )
    .expect("valid member dump")
}

/// `warnings_with_lines` against an explicit DB (the shared harness pins `mini_native()`).
fn warnings_with_lines_in(
    src: &str,
    policy: &WarnPolicy,
    native: &NativeDb,
) -> Vec<(WarningCode, u32)> {
    let tree = gd_syntax::parse(src).tree;
    let result = gd_analyze::analyze(&tree, None, "t.gd", native, &NoCrossFile, policy);
    result
        .diagnostics
        .iter()
        .filter_map(|d| {
            let code = d.warning_code()?;
            let line = 1 + src.as_bytes()[..d.span().start.min(src.len())]
                .iter()
                .filter(|&&b| b == b'\n')
                .count() as u32;
            Some((code, line))
        })
        .collect()
}

/// Non-warning diagnostic messages (errors), for the reduce_call companion assertions.
fn errors_in(src: &str, native: &NativeDb) -> Vec<String> {
    let tree = gd_syntax::parse(src).tree;
    let result = gd_analyze::analyze(&tree, None, "t.gd", native, &NoCrossFile, &godot_policy());
    result
        .diagnostics
        .iter()
        .filter(|d| d.warning_code().is_none())
        .map(|d| d.message().to_owned())
        .collect()
}

#[test]
fn unsafe_property_access_fires_on_typed_native_miss() {
    // analyzer.cpp:4880-4884: attribute unset, base non-meta — warn anchored at the SUBSCRIPT,
    // symbols [attribute name, base_type.to_string()].
    let src = "extends Node\nfunc f(n: Node2D) -> void:\n\tvar x = n.nope\n\tprint_debug(x)\n";
    let policy = policy_enabling(&["UNSAFE_PROPERTY_ACCESS"]);
    let native = member_native();
    assert_eq!(
        warnings_with_lines_in(src, &policy, &native),
        vec![(WarningCode::UnsafePropertyAccess, 3)]
    );
    // The exact upstream template, with the base rendered like Godot's to_string().
    let tree = gd_syntax::parse(src).tree;
    let result = gd_analyze::analyze(&tree, None, "t.gd", &native, &NoCrossFile, &policy);
    let msg = result
        .diagnostics
        .iter()
        .find(|d| d.warning_code() == Some(WarningCode::UnsafePropertyAccess))
        .expect("warning present")
        .message()
        .to_owned();
    assert!(
        msg.contains(
            r#"The property "nope" is not present on the inferred type "Node2D" (but may be present on a subtype)."#
        ),
        "got: {msg}"
    );
}

#[test]
fn unsafe_property_access_silent_on_every_resolving_member_kind() {
    // Property / signal / method-reference / constant / enum value, all through the inherits
    // chain (Node2D declares none of them), plus the CLASS-branch native tail for implicit
    // self (`await self.renamed` — the await_with_signals_no_warning.gd shape).
    let src = "extends Node\n\
               func f(n: Node2D) -> void:\n\
               \tvar a = n.mode\n\
               \tvar b = n.renamed\n\
               \tvar c = n.get_name\n\
               \tvar d = n.NOTIF_READY\n\
               \tvar e = n.KIND_A\n\
               \tawait self.renamed\n\
               \tprint_debug([a, b, c, d, e])\n";
    let policy = policy_enabling(&["UNSAFE_PROPERTY_ACCESS"]);
    let got = warnings_with_lines_in(src, &policy, &member_native());
    assert!(
        !got.iter()
            .any(|(c, _)| *c == WarningCode::UnsafePropertyAccess),
        "all members resolve — no UNSAFE_PROPERTY_ACCESS, got {got:?}"
    );
}

#[test]
fn unsafe_property_access_gated_to_exact_provenance() {
    // docs/02 §11b: a Generic (stock-fallback) DB cannot disprove a custom build's member.
    let src = "extends Node\nfunc f(n: Node2D) -> void:\n\tvar x = n.nope\n\tprint_debug(x)\n";
    let policy = policy_enabling(&["UNSAFE_PROPERTY_ACCESS"]);
    let mut native = member_native();
    native.set_provenance(gd_types::ApiProvenance::Generic);
    assert!(
        warnings_with_lines_in(src, &policy, &native).is_empty(),
        "native-rooted negative under Generic provenance must stay silent"
    );
}

#[test]
fn unsafe_property_access_silent_on_unresolvable_chain_root() {
    // `extends UnknownForkClass`: the chain's native root is unresolvable, so the member
    // surface is incomplete — never warn, even under Exact provenance. (The extends miss
    // itself is a separate, already-pinned error family.)
    let src =
        "extends UnknownForkClass\nfunc f() -> void:\n\tvar x = self.anything\n\tprint_debug(x)\n";
    let policy = policy_enabling(&["UNSAFE_PROPERTY_ACCESS"]);
    let got = warnings_with_lines_in(src, &policy, &member_native());
    assert!(
        !got.iter()
            .any(|(c, _)| *c == WarningCode::UnsafePropertyAccess),
        "unresolvable chain root must not warn, got {got:?}"
    );
}

#[test]
fn unsafe_property_access_silent_on_builtin_base() {
    // Builtin instance miss: upstream's `valid = kind != BUILTIN` excludes builtins from the
    // warning (a builtin's member surface is closed, but Godot still declines the warning here).
    let src = "extends Node\n\
               func f() -> void:\n\
               \tvar v: Vector2\n\
               \tvar a = v.nope\n\
               \tprint_debug(a)\n";
    let policy = policy_enabling(&["UNSAFE_PROPERTY_ACCESS"]);
    let got = warnings_with_lines_in(src, &policy, &member_native());
    assert!(
        !got.iter()
            .any(|(c, _)| *c == WarningCode::UnsafePropertyAccess),
        "a builtin base never warns, got {got:?}"
    );
}

#[test]
fn unsafe_property_access_fires_on_dollar_node_miss() {
    // M11 Phase 2 convergence (docs/02 §11): `$Child` types as bare `NATIVE Node`
    // (analyzer.cpp:3882-3886), so a member miss on it raises UNSAFE_PROPERTY_ACCESS exactly like
    // any other typed node base — `anything` is not on `Node`. (Pre-M11 the permissive deferred-node
    // type silenced this; that was a deliberate deviation, now removed to match Godot.)
    let src = "extends Node\n\
               func f() -> void:\n\
               \tvar b = $Child.anything\n\
               \tprint_debug(b)\n";
    let policy = policy_enabling(&["UNSAFE_PROPERTY_ACCESS"]);
    let got = warnings_with_lines_in(src, &policy, &member_native());
    assert!(
        got.iter()
            .any(|(c, line)| *c == WarningCode::UnsafePropertyAccess && *line == 3),
        "a `$Child` member miss fires UNSAFE_PROPERTY_ACCESS on the bare-Node base, got {got:?}"
    );
}

#[test]
fn native_calls_through_class_base_stay_clean() {
    // The mandatory reduce_call companion (#32): with the CLASS-branch native tail live,
    // `self.get_name()` must bind the native signature — NOT re-reduce the callee to a
    // Callable value and emit `Name "get_name" is a Callable...`.
    let src = "extends Node\nfunc f() -> void:\n\tvar n = self.get_name()\n\tprint_debug(n)\n";
    assert!(
        errors_in(src, &member_native()).is_empty(),
        "a native method call through the class base is clean"
    );
    let policy = policy_enabling(&["UNSAFE_PROPERTY_ACCESS"]);
    assert!(
        warnings_with_lines_in(src, &policy, &member_native()).is_empty(),
        "...and not UNSAFE_PROPERTY_ACCESS either"
    );
}

#[test]
fn native_property_called_as_function_keeps_upstream_error() {
    // The positive-claim counterpart: a property invoked as a function reaches the value-callable
    // path and emits upstream's `Name "X" called as a function but is a "Y".`.
    let src = "extends Node\nfunc f() -> void:\n\tself.mode()\n";
    let errors = errors_in(src, &member_native());
    assert!(
        errors
            .iter()
            .any(|m| m.contains(r#"Name "mode" called as a function"#)),
        "got: {errors:?}"
    );
}

// --- M11 Phase 2: bare-`Node` `$`/`%` typing — the convergence battery ------------------------
//
// `reduce_get_node` types a valid `$`/`%` access as a hard `NATIVE Node` (analyzer.cpp:3882-3886),
// derived from the enclosing class/function ALONE — it does NOT read the scene (`scene_node_facts`
// stays dormant), so these cases reproduce the real `$x` type under `NoCrossFile` with no scene
// fixture. Each assertion was confirmed against the real Godot 4.6.3 binary. The DB gives `Node` a
// member surface (`get_parent`/`add_child` methods, a `name` property) and a sibling subtype
// `Control` so a `Node` → `Control` downcast is a genuine cross-hierarchy move Godot tolerates.

/// `Object ← Node ← CanvasItem ← {Node2D, Control}`. `Node` carries one property (`name`) and two
/// methods (`get_parent` returning `Node`, `add_child(Node)`), enough to exercise valid-vs-miss
/// member access and a typed-arg call on the bare-`Node` `$`/`%` type.
fn scene_node_native() -> NativeDb {
    NativeDb::from_json(
        r#"{
            "header": {"version_major": 4, "version_minor": 6, "version_patch": 3},
            "classes": [
                {"name": "Object"},
                {"name": "Node", "inherits": "Object",
                 "properties": [{"name": "name", "type": "StringName", "setter": "set_name", "getter": "get_name"}],
                 "methods": [
                    {"name": "get_parent", "is_const": true, "is_static": false, "is_vararg": false,
                     "is_virtual": false, "hash": 1, "return_value": {"type": "Node"}, "arguments": []},
                    {"name": "add_child", "is_const": false, "is_static": false, "is_vararg": false,
                     "is_virtual": false, "hash": 2, "return_value": {"type": "void"},
                     "arguments": [{"name": "node", "type": "Node"}]}
                 ]},
                {"name": "CanvasItem", "inherits": "Node"},
                {"name": "Node2D", "inherits": "CanvasItem"},
                {"name": "Control", "inherits": "CanvasItem"}
            ]
        }"#,
    )
    .expect("valid scene-node dump")
}

#[test]
fn dollar_property_read_miss_fires_unsafe_property_access() {
    // `$x.<property read miss>` → UNSAFE_PROPERTY_ACCESS (bare `Node` has no `bogus`).
    let src = "extends Node\nfunc f() -> void:\n\tvar v = $Child.bogus\n\tprint_debug(v)\n";
    let policy = policy_enabling(&["UNSAFE_PROPERTY_ACCESS"]);
    let got = warnings_with_lines_in(src, &policy, &scene_node_native());
    assert!(
        got.iter()
            .any(|(c, _)| *c == WarningCode::UnsafePropertyAccess),
        "a `$Child` property-read miss must fire UNSAFE_PROPERTY_ACCESS, got {got:?}"
    );
}

#[test]
fn dollar_property_write_miss_fires_unsafe_property_access() {
    // `$x.<property write miss>` (`$x.bogus = 5`) → UNSAFE_PROPERTY_ACCESS. The assignment-LHS
    // subscript reduction must reach the same unsafe-property site as a read.
    let src = "extends Node\nfunc f() -> void:\n\t$Child.bogus = 5\n";
    let policy = policy_enabling(&["UNSAFE_PROPERTY_ACCESS"]);
    let got = warnings_with_lines_in(src, &policy, &scene_node_native());
    assert!(
        got.iter()
            .any(|(c, _)| *c == WarningCode::UnsafePropertyAccess),
        "a `$Child` property-write miss must fire UNSAFE_PROPERTY_ACCESS, got {got:?}"
    );
}

#[test]
#[ignore = "blocked on #123: UNSAFE_METHOD_ACCESS under-emits on native-base method misses \
            (it only fires for the untyped-param Variant shape) — a general M3 analyzer gap, NOT \
            $-specific (a plain `var n: Node; n.miss()` is silent too). Godot fires it here \
            (analyzer.cpp:3741). Assertion is the correct convergence target, ignored until #123."]
fn dollar_method_miss_fires_unsafe_method_access() {
    // `$x.<method miss>()` → UNSAFE_METHOD_ACCESS (`Node` has no `bogus_method`). Godot
    // (analyzer.cpp:3741) warns on a non-self, non-builtin-hard base miss; bare `Node` qualifies.
    let src = "extends Node\nfunc f() -> void:\n\t$Child.bogus_method()\n";
    let policy = policy_enabling(&["UNSAFE_METHOD_ACCESS"]);
    let got = warnings_with_lines_in(src, &policy, &scene_node_native());
    assert!(
        got.iter()
            .any(|(c, _)| *c == WarningCode::UnsafeMethodAccess),
        "a `$Child` method miss must fire UNSAFE_METHOD_ACCESS, got {got:?}"
    );
}

#[test]
fn dollar_valid_node_method_is_silent() {
    // `$x.get_parent()` (a real `Node` method) → silent under both unsafe-access warnings enabled.
    let src = "extends Node\nfunc f() -> void:\n\tvar p = $Child.get_parent()\n\tprint_debug(p)\n";
    let policy = policy_enabling(&["UNSAFE_PROPERTY_ACCESS", "UNSAFE_METHOD_ACCESS"]);
    let got = warnings_with_lines_in(src, &policy, &scene_node_native());
    assert!(
        !got.iter().any(|(c, _)| matches!(
            c,
            WarningCode::UnsafePropertyAccess | WarningCode::UnsafeMethodAccess
        )),
        "a valid `Node` method call on `$Child` must be silent, got {got:?}"
    );
}

#[test]
fn dollar_walrus_infers_node_no_inference_on_variant() {
    // `var y := $Child` must infer a hard `Node` (the bare-Node type), NOT trip
    // INFERENCE_ON_VARIANT — an error-by-default warning. The old permissive `Variant` seam was
    // specifically tuned to dodge this; bare `Node` is a hard type, so `:=` infers it cleanly.
    let src = "extends Node\nfunc f() -> void:\n\tvar y := $Child\n\tprint_debug(y)\n\tvar z := get_node(^\"Child\")\n\tprint_debug(z)\n";
    let policy = policy_enabling(&["INFERENCE_ON_VARIANT"]);
    let got = warnings_with_lines_in(src, &policy, &scene_node_native());
    assert!(
        !got.iter().any(|(c, _)| *c == WarningCode::InferenceOnVariant),
        "`var y := $Child` / `:= get_node(...)` must infer a hard Node, not fire INFERENCE_ON_VARIANT, got {got:?}"
    );
}

#[test]
fn dollar_sibling_downcast_assignment_is_silent() {
    // `var c: Control = $x` → SILENT: Godot tolerates the `Node` → `Control` downcast (gradual
    // typing, an unsafe assignment, not an error). A precise scene type would reject this sibling
    // downcast — the false positive this whole design avoids. Asserts NO assignment ERROR.
    let src = "extends Node\nfunc f() -> void:\n\tvar c: Control = $Child\n\tprint_debug(c)\n";
    let errors = errors_in(src, &scene_node_native());
    assert!(
        !errors.iter().any(|m| m.contains("Cannot assign")),
        "a `Node` → `Control` downcast from `$Child` must NOT error (Godot tolerates it), got {errors:?}"
    );
}

#[test]
fn dollar_cast_is_silent() {
    // `$x as Control` → silent: with `$x` a hard `Node` (not Variant), the operand is no longer the
    // Variant/soft case that fires UNSAFE_CAST, and `Node as Control` is a valid downcast.
    let src = "extends Node\nfunc f() -> void:\n\tvar c = $Child as Control\n\tprint_debug(c)\n";
    let policy = policy_enabling(&["UNSAFE_CAST"]);
    let got = warnings_with_lines_in(src, &policy, &scene_node_native());
    assert!(
        !got.iter().any(|(c, _)| *c == WarningCode::UnsafeCast),
        "`$Child as Control` must be silent (the operand is a hard `Node`, not Variant), got {got:?}"
    );
    assert!(
        errors_in(src, &scene_node_native()).is_empty(),
        "`$Child as Control` is a valid downcast — no cast error"
    );
}

#[test]
fn dollar_typed_arg_pass_fires_unsafe_call_argument() {
    // `wants($x)` where `wants(p: Control)` → UNSAFE_CALL_ARGUMENT: a bare `Node` (supertype) is
    // passed where a `Control` (subtype) is required — Godot's unsafe-downcast-argument warning.
    let src = "extends Node\n\
               func wants(_c: Control) -> void:\n\
               \tpass\n\
               func f() -> void:\n\
               \twants($Child)\n";
    let policy = policy_enabling(&["UNSAFE_CALL_ARGUMENT"]);
    let got = warnings_with_lines_in(src, &policy, &scene_node_native());
    assert!(
        got.iter()
            .any(|(c, _)| *c == WarningCode::UnsafeCallArgument),
        "passing bare-`Node` `$Child` to a `Control` parameter must fire UNSAFE_CALL_ARGUMENT, got {got:?}"
    );
}
