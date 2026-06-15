//! Tests for `ParseTree::locals_in_scope_at` / `innermost_suite_at` (M8 Phase 1, #64, Group 4):
//! reconstructing the in-scope local/parameter set at a byte offset from the parse tree's retained
//! scope structure (`SuiteNode.locals` + `parent_block`). Uses the real parser so suites, locals,
//! parameters, and spans are exactly what production sees.

use gd_syntax::ast::{Local, LocalKind, NodeKind};
use gd_syntax::parse;

/// The byte offset immediately after the first occurrence of `needle` in `src` — a cursor
/// position pinned to a real source landmark, so the test is robust to whitespace edits.
fn after(src: &str, needle: &str) -> usize {
    let i = src
        .find(needle)
        .unwrap_or_else(|| panic!("needle {needle:?} in fixture"));
    i + needle.len()
}

fn names<'a>(locals: &'a [&Local]) -> Vec<&'a str> {
    locals.iter().map(|l| l.name.as_str()).collect()
}

const FIXTURE: &str = "\
func outer(param_a: int, param_b: String) -> void:
\tvar outer_local := 1
\tif param_a > 0:
\t\tvar inner_local := 2
\t\tinner_local += 1
\t\tvar not_yet := 3
\tvar sibling := 4
";

#[test]
fn locals_in_scope_inside_nested_block_is_inner_plus_outer_plus_params() {
    let tree = parse(FIXTURE).tree;
    // Cursor on the `inner_local += 1` line: inside the `if` block, after `inner_local` and
    // `outer_local` / params are declared, but before `not_yet` (same block, later) and `sibling`
    // (outer block, later).
    let byte = after(FIXTURE, "inner_local += 1");
    let in_scope = tree.locals_in_scope_at(byte);
    let got = names(&in_scope);

    // Inner-block local (declared before the cursor).
    assert!(
        got.contains(&"inner_local"),
        "inner_local in scope: {got:?}"
    );
    // Outer-block local (declared before the inner block).
    assert!(
        got.contains(&"outer_local"),
        "outer_local in scope: {got:?}"
    );
    // Parameters (recorded as locals on the function body suite).
    assert!(got.contains(&"param_a"), "param_a in scope: {got:?}");
    assert!(got.contains(&"param_b"), "param_b in scope: {got:?}");

    // Not-yet-declared in the SAME block (after the cursor) — excluded.
    assert!(
        !got.contains(&"not_yet"),
        "not_yet is declared after the cursor and must be excluded: {got:?}"
    );
    // Declared later in the OUTER block (after the cursor) — excluded.
    assert!(
        !got.contains(&"sibling"),
        "sibling is declared after the cursor and must be excluded: {got:?}"
    );

    // Parameters carry the Parameter kind; the function-body / for / pattern bindings all flow
    // through `locals`, so no separate parameter pass is needed.
    let param_a = in_scope.iter().find(|l| l.name == "param_a").unwrap();
    assert_eq!(param_a.kind, LocalKind::Parameter);
    let outer_local = in_scope.iter().find(|l| l.name == "outer_local").unwrap();
    assert_eq!(outer_local.kind, LocalKind::Variable);

    // Exactly the expected set (no extras, no out-of-scope leakage).
    let mut sorted = got.clone();
    sorted.sort_unstable();
    assert_eq!(
        sorted,
        vec!["inner_local", "outer_local", "param_a", "param_b"],
        "the in-scope set is precisely inner + outer + params"
    );
}

#[test]
fn locals_in_scope_in_outer_block_excludes_inner_block_locals() {
    let tree = parse(FIXTURE).tree;
    // Cursor on the `var sibling := 4` line in the OUTER block: inner_local / not_yet (in the
    // nested `if` block) are out of scope; outer_local and params are in scope.
    let byte = after(FIXTURE, "var sibling := 4");
    let in_scope = tree.locals_in_scope_at(byte);
    let got = names(&in_scope);
    assert!(got.contains(&"outer_local"), "outer_local: {got:?}");
    assert!(got.contains(&"param_a"), "param_a: {got:?}");
    assert!(
        !got.contains(&"inner_local"),
        "inner_local belongs to the nested block, out of scope here: {got:?}"
    );
    // `sibling` itself: the cursor is right after the declaration text, so it has just been
    // declared — included (`end <= byte`).
    assert!(got.contains(&"sibling"), "sibling just declared: {got:?}");
}

#[test]
fn for_loop_variable_is_in_scope() {
    const SRC: &str = "\
func f() -> void:
\tfor item in [1, 2, 3]:
\t\titem
";
    let tree = parse(SRC).tree;
    let byte = after(SRC, "\t\titem");
    let in_scope = tree.locals_in_scope_at(byte);
    let got = names(&in_scope);
    assert!(
        got.contains(&"item"),
        "the `for` loop variable is in scope in the loop body: {got:?}"
    );
    let item = tree
        .locals_in_scope_at(byte)
        .into_iter()
        .find(|l| l.name == "item")
        .unwrap();
    assert_eq!(item.kind, LocalKind::ForVariable);
}

#[test]
fn innermost_suite_at_finds_the_enclosing_block() {
    let tree = parse(FIXTURE).tree;
    let byte = after(FIXTURE, "inner_local += 1");
    let suite = tree
        .innermost_suite_at(byte)
        .expect("a block encloses the cursor");
    assert!(
        matches!(tree.get(suite).kind, NodeKind::Suite(_)),
        "innermost_suite_at returns a Suite node"
    );
    // The innermost block is the `if` body — it has `inner_local` (and `not_yet`) as direct
    // locals, but NOT `outer_local` (that is the parent block's).
    let NodeKind::Suite(s) = &tree.get(suite).kind else {
        unreachable!()
    };
    let direct: Vec<&str> = s.locals.iter().map(|l| l.name.as_str()).collect();
    assert!(
        direct.contains(&"inner_local"),
        "if-body locals: {direct:?}"
    );
    assert!(
        !direct.contains(&"outer_local"),
        "outer_local is the parent block's: {direct:?}"
    );
}

#[test]
fn no_block_at_class_scope_yields_empty() {
    // A byte at top-level class scope (not inside any function body) has no enclosing suite.
    let src = "var member := 1\n";
    let tree = parse(src).tree;
    let byte = 0; // the very start, class scope
    assert!(
        tree.locals_in_scope_at(byte).is_empty(),
        "no locals are in scope at class scope"
    );
    assert!(tree.innermost_suite_at(byte).is_none());
}

// =================================================================================================
// `resolve_local_binding_at` / `local_binding_occurrences` (#107): precise, binding-based local
// resolution — the substrate the LSP rename / documentHighlight local path consumes so for-loop and
// match-pattern binds resolve, and inner-scope shadows are kept distinct from their same-named
// siblings.
// =================================================================================================

use gd_syntax::ByteSpan;

/// The span of the smallest `Function` node containing `byte` — the enclosing-function bound a
/// `local_binding_occurrences` search uses (a `for`/`match` declaration token lives outside its body
/// block, so a block-scoped bound would drop it).
fn enclosing_fn_span(tree: &gd_syntax::ParseTree, byte: usize) -> ByteSpan {
    let mut best: Option<ByteSpan> = None;
    for id in tree.iter_ids() {
        if matches!(tree.get(id).kind, NodeKind::Function(_)) {
            let s = tree.get(id).span;
            if s.start <= byte
                && byte < s.end
                && best.is_none_or(|b| s.end - s.start < b.end - b.start)
            {
                best = Some(s);
            }
        }
    }
    best.expect("an enclosing function span")
}

/// The set of `(start, end)` occurrence extents for the binding the cursor at `cursor` resolves to,
/// searched within its enclosing function — sorted by start for stable assertions.
fn occurrences_at(tree: &gd_syntax::ParseTree, cursor: usize, name: &str) -> Vec<(usize, usize)> {
    let decl = tree
        .resolve_local_binding_at(cursor, name)
        .expect("cursor resolves to a local binding");
    let fn_span = enclosing_fn_span(tree, cursor);
    let mut got: Vec<(usize, usize)> = tree
        .local_binding_occurrences(decl, fn_span)
        .into_iter()
        .map(|s| (s.start, s.end))
        .collect();
    got.sort_unstable();
    got
}

#[test]
fn resolve_for_loop_variable_use_and_decl_agree() {
    const SRC: &str = "func f() -> void:\n\tfor i in [1, 2]:\n\t\tprint(i)\n\t\ti = 0\n";
    let tree = parse(SRC).tree;
    // The use inside the body and the declaration token resolve to the SAME binding identifier.
    let use_byte = after(SRC, "print(i"); // just past the `i` in print(i)
    let decl = tree
        .resolve_local_binding_at(use_byte - 1, "i")
        .expect("for-var use resolves");
    // Clicking the declaration token (`for i`) resolves to the same identifier.
    let decl_byte = after(SRC, "for "); // the start of the `i` after `for `
    let from_decl = tree
        .resolve_local_binding_at(decl_byte, "i")
        .expect("for-var decl resolves");
    assert_eq!(decl, from_decl, "use and decl resolve to one binding");
    // Occurrences = the decl token + both body uses (3 total).
    let occ = occurrences_at(&tree, decl_byte, "i");
    assert_eq!(occ.len(), 3, "decl + 2 uses: {occ:?}");
}

#[test]
fn resolve_match_pattern_bind() {
    const SRC: &str = "func f(v) -> void:\n\tmatch v:\n\t\tvar n:\n\t\t\tprint(n)\n";
    let tree = parse(SRC).tree;
    let bind_byte = after(SRC, "\t\tvar "); // the start of the `n` of the bind
    let decl = tree
        .resolve_local_binding_at(bind_byte, "n")
        .expect("pattern bind decl resolves");
    let use_byte = after(SRC, "print(n") - 1;
    let from_use = tree
        .resolve_local_binding_at(use_byte, "n")
        .expect("pattern bind use resolves");
    assert_eq!(decl, from_use, "bind + use are one binding");
    let occ = occurrences_at(&tree, bind_byte, "n");
    assert_eq!(occ.len(), 2, "bind site + branch-body use: {occ:?}");
}

#[test]
fn resolve_inner_shadow_keeps_bindings_distinct() {
    // Outer `x` (decl + use before the inner block) and an inner-block `var x` (decl + use) are two
    // distinct bindings — resolution and occurrence sets must not bleed across.
    const SRC: &str =
        "func f() -> void:\n\tvar x = 1\n\tprint(x)\n\tif true:\n\t\tvar x = 2\n\t\tprint(x)\n";
    let tree = parse(SRC).tree;
    let outer_decl_byte = after(SRC, "var x = 1").saturating_sub("x = 1".len()); // on the outer `x`
    let outer = tree
        .resolve_local_binding_at(outer_decl_byte, "x")
        .expect("outer decl");
    let inner_decl_byte = after(SRC, "var x = 2").saturating_sub("x = 2".len()); // on the inner `x`
    let inner = tree
        .resolve_local_binding_at(inner_decl_byte, "x")
        .expect("inner decl");
    assert_ne!(outer, inner, "inner and outer are distinct bindings");

    // The use BEFORE the inner block resolves to the OUTER binding; the use AFTER resolves to inner.
    let pre_use = after(SRC, "print(x)\n\tif"); // first print(x), before the if-block
    let pre = tree
        .resolve_local_binding_at(pre_use - "print(x)\n\tif".len() + "print(".len(), "x")
        .expect("pre-block use");
    assert_eq!(
        pre, outer,
        "the use before the inner var resolves to the outer binding"
    );

    // Outer occurrences = outer decl + the single pre-block use (2), never the inner pair.
    let outer_occ = occurrences_at(&tree, outer_decl_byte, "x");
    assert_eq!(
        outer_occ.len(),
        2,
        "outer = decl + 1 use, not the inner pair: {outer_occ:?}"
    );
    // Inner occurrences = inner decl + its use (2), never the outer pair.
    let inner_occ = occurrences_at(&tree, inner_decl_byte, "x");
    assert_eq!(
        inner_occ.len(),
        2,
        "inner = decl + 1 use, not the outer pair: {inner_occ:?}"
    );
    // The two sets are disjoint.
    for o in &outer_occ {
        assert!(
            !inner_occ.contains(o),
            "occurrence sets must be disjoint: {o:?}"
        );
    }
}

#[test]
fn resolve_lambda_capture_binds_to_outer() {
    // A use captured inside a lambda body resolves to the OUTER binding (the lambda body's
    // parent_block chains to the enclosing suite).
    const SRC: &str = "func f() -> void:\n\tvar c = 1\n\tvar g = func(): return c\n";
    let tree = parse(SRC).tree;
    let cap_use = after(SRC, "return c") - 1; // on the captured `c`
    let from_cap = tree
        .resolve_local_binding_at(cap_use, "c")
        .expect("lambda capture resolves");
    let decl_byte = after(SRC, "var c").saturating_sub("c".len());
    let decl = tree
        .resolve_local_binding_at(decl_byte, "c")
        .expect("outer decl");
    assert_eq!(
        from_cap, decl,
        "the lambda capture resolves to the outer binding"
    );
}

#[test]
fn resolve_self_reference_in_initializer_resolves_outward() {
    // `var x = x` — the initializer `x` must NOT resolve to the binding being declared (it isn't
    // visible inside its own initializer); with no outer `x` it resolves to no local binding.
    const SRC: &str = "func f() -> void:\n\tvar x = x\n";
    let tree = parse(SRC).tree;
    let init_use = after(SRC, "var x = x"); // just past the initializer `x`
    assert_eq!(
        tree.resolve_local_binding_at(init_use - 1, "x"),
        None,
        "the initializer self-reference does not resolve to the binding being declared"
    );
}

#[test]
fn member_or_attribute_identifier_is_not_a_local() {
    // A member declared at class scope and accessed as `self.member` is not a local; resolution
    // returns None, and the attribute occurrence is excluded from a same-named local's set.
    const SRC: &str =
        "var member := 0\nfunc f() -> void:\n\tvar member := 1\n\tself.member = member\n";
    let tree = parse(SRC).tree;
    // The LOCAL `member` (decl on the `var member := 1` line).
    let local_decl = after(SRC, "var member := 1").saturating_sub("member := 1".len());
    assert!(
        tree.resolve_local_binding_at(local_decl, "member")
            .is_some(),
        "the local member resolves to a binding"
    );
    let occ = occurrences_at(&tree, local_decl, "member");
    // decl + the bare `member` read on the RHS of `self.member = member` (2) — the `self.member`
    // attribute on the LHS is excluded (it is the class member, not the local).
    assert_eq!(
        occ.len(),
        2,
        "local decl + bare RHS use, not the self.member attribute: {occ:?}"
    );
}

#[test]
fn lua_style_dict_key_is_not_a_local_occurrence() {
    // `{ name = value }` — the Lua-style KEY is a folded string literal, NOT a reference to the local
    // `name`. It must be excluded from the occurrence set (renaming it would silently change the key
    // string). The dict VALUE reference (`other = name`) IS a real use and stays.
    const SRC: &str =
        "func f() -> void:\n\tvar name = 1\n\tvar d = { name = 2, other = name }\n\tprint(name)\n";
    let tree = parse(SRC).tree;
    let decl_byte = after(SRC, "\tvar "); // start of the `name` declaration
    let occ = occurrences_at(&tree, decl_byte, "name");
    // decl + the dict VALUE ref + print — NOT the Lua key (3, not 4).
    assert_eq!(
        occ.len(),
        3,
        "local decl + dict value ref + print, excluding the Lua-style key: {occ:?}"
    );
}

#[test]
fn lua_style_single_element_ambiguous_dict_key_excluded() {
    // The single-element ambiguous case `{ key = key }` is parsed Lua-style (style == None): the KEY
    // is a string literal (excluded), the VALUE is a real reference (kept).
    const SRC: &str = "func f() -> void:\n\tvar key = 1\n\tvar d = { key = key }\n";
    let tree = parse(SRC).tree;
    let decl_byte = after(SRC, "\tvar ");
    let occ = occurrences_at(&tree, decl_byte, "key");
    assert_eq!(
        occ.len(),
        2,
        "local decl + the value reference, excluding the ambiguous Lua key: {occ:?}"
    );
}

#[test]
fn python_style_dict_string_key_leaves_value_reference() {
    // `{ "name": name }` — a Python-style key is a string LITERAL (not an identifier), so nothing to
    // exclude there; the VALUE `name` is a real reference and is collected.
    const SRC: &str =
        "func f() -> void:\n\tvar name = 1\n\tvar d = { \"name\": name }\n\tprint(name)\n";
    let tree = parse(SRC).tree;
    let decl_byte = after(SRC, "\tvar ");
    let occ = occurrences_at(&tree, decl_byte, "name");
    assert_eq!(
        occ.len(),
        3,
        "local decl + dict value ref + print (the string key is a literal, not an ident): {occ:?}"
    );
}
