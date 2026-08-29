//! Each guarded 4.6 → 4.7 frontend difference, pinned at both tags.
//!
//! The conformance corpus is the real regression net, but it only ever runs one tag's goldens at a
//! time, so a guard that silently collapsed to a single behavior would still pass it. These tests
//! assert the *difference*: every case states what 4.6 does and what 4.7 does, and inverting a
//! guard fails at least one of them.
//!
//! The documented no-ops (Godot's `cursor_position` removal, the `-1` extent sentinel, the
//! `ParserError` range, and the warning columns) have nothing to pin here — see the delta table in
//! `docs/02-frontend-port.md` §11c for why.

use gd_syntax::{tokenize_with_dialect, Dialect, ParseOptions};

fn parse_in(source: &str, dialect: Dialect) -> gd_syntax::ParseResult {
    gd_syntax::parse_with_options(
        source,
        &ParseOptions {
            dialect,
            ..Default::default()
        },
    )
}

fn messages(source: &str, dialect: Dialect) -> Vec<String> {
    parse_in(source, dialect)
        .diagnostics
        .into_iter()
        .map(|d| d.message)
        .collect()
}

/// The Godot-space 1-based column each token starts at.
fn start_columns(source: &str, dialect: Dialect) -> Vec<u32> {
    let (tokens, _) = tokenize_with_dialect(source, dialect);
    tokens.iter().map(|t| t.loc.start.column).collect()
}

// ---------------------------------------------------------------------------------------------------
// 1. A tab advances `column` by 1 instead of `tab_size`.
// ---------------------------------------------------------------------------------------------------

#[test]
fn a_tab_is_one_column_at_4_7_and_a_tab_stop_at_4_6() {
    // `func f():\n\tpass\n` — the `pass` sits after one leading tab.
    let src = "func f():\n\tpass\n";
    let at_46 = start_columns(src, Dialect::Godot4_6);
    let at_47 = start_columns(src, Dialect::Godot4_7);
    assert_ne!(
        at_46, at_47,
        "the tab-width guard collapsed to one behavior"
    );
    // Godot's tab size is 4, so 4.6 puts `pass` at column 5 and 4.7 puts it at column 2.
    assert!(at_46.contains(&5), "4.6 columns: {at_46:?}");
    assert!(at_47.contains(&2), "4.7 columns: {at_47:?}");
}

#[test]
fn a_tab_mid_line_shifts_only_the_column_not_the_indent_depth() {
    // Two statements at the same indent depth must both parse cleanly under both dialects — the
    // change is to `column`, never to `indent_count`.
    let src = "func f():\n\tvar a = 1\n\tvar b = 2\n";
    for d in [Dialect::Godot4_6, Dialect::Godot4_7] {
        assert!(
            parse_in(src, d).diagnostics.is_empty(),
            "dialect {d:?} reported errors"
        );
    }
    assert_ne!(
        start_columns(src, Dialect::Godot4_6),
        start_columns(src, Dialect::Godot4_7)
    );
}

// ---------------------------------------------------------------------------------------------------
// 2. Token position fields default to `1` instead of `0`.
// ---------------------------------------------------------------------------------------------------

/// The head class node's Godot-space extents, which `reset_extents_from_previous` stamps from
/// `previous` before any token has been consumed.
fn root_loc(source: &str, dialect: Dialect) -> gd_syntax::LineColRange {
    let tree = parse_in(source, dialect).tree;
    let root = tree.root_id().expect("root");
    tree.get(root).loc
}

#[test]
fn the_empty_token_positions_the_root_at_one_one_at_4_7() {
    // An empty source never advances past a real token, so the root's extents come straight from
    // the default-constructed `previous`. 4.6 left it at 0/0 — Godot's own comment calls that
    // reading uninitialized memory — and 4.7 defaults it to 1/1.
    let at_46 = root_loc("", Dialect::Godot4_6);
    let at_47 = root_loc("", Dialect::Godot4_7);
    assert_eq!((at_46.start.line, at_46.start.column), (0, 0));
    assert_eq!((at_47.start.line, at_47.start.column), (1, 1));
}

#[test]
fn a_non_empty_source_positions_the_root_identically_across_the_tags() {
    // Once a real token is consumed, `previous` is real and the default never shows through.
    let src = "extends Node
var x = 1
";
    assert_eq!(
        root_loc(src, Dialect::Godot4_6),
        root_loc(src, Dialect::Godot4_7)
    );
}

// ---------------------------------------------------------------------------------------------------
// 6. `class_name` is rejected in a built-in script.
// ---------------------------------------------------------------------------------------------------

const BUILT_IN_ERROR: &str = r#""class_name" isn't allowed in built-in scripts."#;

fn parse_at_path(source: &str, path: &str, dialect: Dialect) -> Vec<String> {
    gd_syntax::parse_with_options(
        source,
        &ParseOptions {
            dialect,
            script_path: path,
        },
    )
    .diagnostics
    .into_iter()
    .map(|d| d.message)
    .collect()
}

#[test]
fn class_name_in_a_built_in_script_is_an_error_only_at_4_7() {
    let src = "class_name Player\nextends Node\n";
    let path = "res://main.tscn::GDScript_abcde";
    assert!(!parse_at_path(src, path, Dialect::Godot4_6).contains(&BUILT_IN_ERROR.to_string()));
    assert!(parse_at_path(src, path, Dialect::Godot4_7).contains(&BUILT_IN_ERROR.to_string()));
}

#[test]
fn class_name_in_a_real_file_is_never_the_built_in_error() {
    let src = "class_name Player\nextends Node\n";
    for path in ["res://player.gd", "", "/tmp/scratch.gd", "user://x.gd"] {
        for d in [Dialect::Godot4_6, Dialect::Godot4_7] {
            let msgs = parse_at_path(src, path, d);
            assert!(
                !msgs.contains(&BUILT_IN_ERROR.to_string()),
                "path {path:?} dialect {d:?} → {msgs:?}"
            );
        }
    }
}

#[test]
fn the_built_in_check_needs_both_the_res_prefix_and_the_double_colon() {
    // Godot's condition is `begins_with("res://") && contains("::")`; a `::` outside `res://` is a
    // plain file path, not an embedded script.
    let src = "class_name Player\n";
    let msgs = parse_at_path(src, "/home/u/weird::name.gd", Dialect::Godot4_7);
    assert!(!msgs.contains(&BUILT_IN_ERROR.to_string()), "{msgs:?}");
}

// ---------------------------------------------------------------------------------------------------
// 7. `super` enters multiline mode only when the `(` is really there.
// ---------------------------------------------------------------------------------------------------

#[test]
fn a_malformed_super_cascades_differently_across_the_tags() {
    // `super` with neither `(` nor `.`: 4.6 pushed multiline mode unconditionally and popped it in
    // the failure arms, which swallows the newline and changes what the parser sees next.
    let src = "func f():\n\tsuper\n\tvar x = 1\n";
    let at_46 = messages(src, Dialect::Godot4_6);
    let at_47 = messages(src, Dialect::Godot4_7);
    assert!(!at_46.is_empty(), "4.6 should report the malformed super");
    assert!(!at_47.is_empty(), "4.7 should report the malformed super");
    assert_ne!(
        at_46, at_47,
        "the super multiline guard collapsed to one behavior: {at_46:?}"
    );
}

#[test]
fn a_well_formed_super_call_is_identical_across_the_tags() {
    for src in [
        "func f():\n\tsuper()\n",
        "func f():\n\tsuper.f()\n",
        "func f():\n\tsuper(\n\t\t1,\n\t\t2,\n\t)\n",
    ] {
        assert!(
            parse_in(src, Dialect::Godot4_6).diagnostics.is_empty(),
            "4.6 errored on {src:?}"
        );
        assert!(
            parse_in(src, Dialect::Godot4_7).diagnostics.is_empty(),
            "4.7 errored on {src:?}"
        );
    }
}

// ---------------------------------------------------------------------------------------------------
// 10 (P2 carry-over). `@warning_ignore` name validation follows the dialect's warning set.
// ---------------------------------------------------------------------------------------------------

#[test]
fn the_4_7_only_warning_name_is_rejected_at_4_6() {
    let src = "@warning_ignore(\"confusable_temporary_modification\")\nvar x = 1\n";
    assert!(
        messages(src, Dialect::Godot4_6)
            .iter()
            .any(|m| m.contains("confusable_temporary_modification")),
        "4.6 must reject a warning name that did not exist yet"
    );
    assert!(
        messages(src, Dialect::Godot4_7).is_empty(),
        "4.7 must accept it: {:?}",
        messages(src, Dialect::Godot4_7)
    );
}
