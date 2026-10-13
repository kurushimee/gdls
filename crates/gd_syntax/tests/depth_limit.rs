//! The nesting-depth limit (`MAX_PARSE_DEPTH`).
//!
//! Godot has no limit and crashes once a file nests deep enough, so the limit sits above every
//! depth Godot can load. Below it gdls must say nothing a Godot load would not; past it, the file
//! gets one error for the too-deep line and every other line still parses.

use gd_syntax::parser::MAX_PARSE_DEPTH;
use gd_syntax::{parse, ParseResult};

const LIMIT: usize = MAX_PARSE_DEPTH as usize;

fn messages(r: &ParseResult) -> Vec<&str> {
    r.diagnostics.iter().map(|d| d.message.as_str()).collect()
}

/// A script whose `f` holds `expr` on one line between two ordinary statements, with a second
/// function after it.
fn in_function(expr: &str) -> String {
    format!("func f():\n\tvar a = 1\n\tvar x = {expr}\n\tvar y = 2\n\nfunc g():\n\tpass\n")
}

fn symbol_names(r: &ParseResult) -> Vec<String> {
    r.symbols[0]
        .children
        .iter()
        .map(|s| s.name.clone())
        .collect()
}

#[test]
fn nesting_just_under_the_limit_is_not_an_error() {
    // Each level is one array, one `not`, or one link of a member chain. The enclosing `var`
    // statement and the function body take a level each, hence the margin.
    let d = LIMIT - 8;
    for expr in [
        format!("{}{}", "[".repeat(d), "]".repeat(d)),
        format!("{}true", "not ".repeat(d)),
        format!("self{}", ".a".repeat(d)),
        format!("1{}", " + 1".repeat(d)),
    ] {
        let r = parse(&in_function(&expr));
        assert_eq!(messages(&r), Vec::<&str>::new());
    }
}

#[test]
fn past_the_limit_the_line_gets_one_error_and_the_rest_still_parses() {
    let d = LIMIT + 10;
    for expr in [
        format!("{}{}", "[".repeat(d), "]".repeat(d)),
        format!("{}true", "not ".repeat(d)),
        format!("self{}", ".a".repeat(d)),
        format!("1{}", " + 1".repeat(d)),
        format!("{}1", "func(): return ".repeat(d)),
    ] {
        let r = parse(&in_function(&expr));
        assert_eq!(r.diagnostics.len(), 1, "{:?}", messages(&r));
        assert!(
            r.diagnostics[0].message.ends_with(" is too deeply nested."),
            "{:?}",
            messages(&r)
        );
        assert_eq!(symbol_names(&r), ["f", "g"]);
    }
}

#[test]
fn a_too_deep_class_variable_does_not_swallow_the_members_after_it() {
    let d = LIMIT + 10;
    for expr in [
        format!("{}{}", "[".repeat(d), "]".repeat(d)),
        format!("{}1", "func(): return ".repeat(d)),
    ] {
        let src = format!("var x = {expr}\nvar y = 2\n\nfunc g():\n\tpass\n");
        let r = parse(&src);
        assert_eq!(r.diagnostics.len(), 1, "{:?}", messages(&r));
        assert_eq!(symbol_names(&r), ["x", "y", "g"]);
    }
}

#[test]
fn a_long_elif_chain_counts_toward_the_limit() {
    // Each `elif` nests the next branch inside the previous one's else-block, in Godot's parser and
    // in its tree, so a long enough chain overflows Godot just as deep brackets do.
    let mut src = String::from("func f(v):\n\tif v == 0:\n\t\tpass\n");
    for i in 1..LIMIT + 10 {
        src.push_str(&format!("\telif v == {i}:\n\t\tpass\n"));
    }
    let r = parse(&src);
    assert!(
        messages(&r)
            .iter()
            .any(|m| m.ends_with(" is too deeply nested.")),
        "{:?}",
        &messages(&r)[..messages(&r).len().min(3)]
    );
}
