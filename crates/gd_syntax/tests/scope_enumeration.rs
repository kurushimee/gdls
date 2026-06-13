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
