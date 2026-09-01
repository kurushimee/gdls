//! `Variant::stringify` for the values the fold table can hold.
//!
//! Godot interpolates a constant's own rendering into two diagnostics — the duplicate-key error
//! (`Key "%s" was already used…`, analyzer.cpp:3831) and the constant-subscript miss (`Cannot get
//! index "%s" from "%s".`, analyzer.cpp:4926) — so matching those messages means matching
//! `Variant::stringify` byte for byte, down to the two spaces an empty dictionary renders as.
//!
//! Two renderings, not one. [`stringify`] is the top level, where a string-like prints as its bare
//! text; [`stringify_clean`] is what a value nested inside a collection gets, where the same string
//! is escaped, quoted, and prefixed with the sigil that names its type (`core/variant/variant.cpp`
//! `stringify_variant_clean`, :1562). `{"a": 1}` renders `{ "a": 1 }` for that reason while the
//! same string as a top-level key renders bare.

use crate::data_type::variant_type_name;
use crate::foldtable::FoldedValue;

/// `Variant::MAX_RECURSION` (`core/variant/variant.cpp`) — the depth at which Godot gives up and
/// prints an ellipsis instead of recursing further.
const MAX_RECURSION: usize = 100;

/// `Variant::stringify(0)` (`core/variant/variant.cpp:1597`).
pub fn stringify(value: &FoldedValue) -> String {
    stringify_at(value, 0)
}

fn stringify_at(value: &FoldedValue, recursion: usize) -> String {
    match value {
        FoldedValue::Nil => "<null>".to_owned(),
        FoldedValue::Bool(b) => if *b { "true" } else { "false" }.to_owned(),
        FoldedValue::Int(i) => i.to_string(),
        FoldedValue::Float(f) => num_real(*f),
        // All three string-likes print as their bare text at the top level; the sigils and quotes
        // belong to `stringify_clean`.
        FoldedValue::String(s) | FoldedValue::StringName(s) | FoldedValue::NodePath(s) => s.clone(),
        FoldedValue::Array(items) => {
            if recursion > MAX_RECURSION {
                return "[...]".to_owned();
            }
            let inner: Vec<String> = items
                .iter()
                .map(|v| stringify_clean(v, recursion + 1))
                .collect();
            format!("[{}]", inner.join(", "))
        }
        FoldedValue::Dictionary(pairs) => {
            if recursion > MAX_RECURSION {
                return "{ ... }".to_owned();
            }
            // The leading and trailing space are Godot's, and they are why an EMPTY dictionary
            // renders as `{  }` with two spaces rather than `{}`.
            let inner: Vec<String> = pairs
                .iter()
                .map(|(k, v)| {
                    format!(
                        "{}: {}",
                        stringify_clean(k, recursion + 1),
                        stringify_clean(v, recursion + 1)
                    )
                })
                .collect();
            format!("{{ {} }}", inner.join(", "))
        }
        // A utility callable stringifies as its scoped name (`GDScriptUtilityCallable::get_as_text`).
        FoldedValue::Opaque(_, Some(util)) => util.as_text(),
        // Every other opaque constant is a value gdls could not materialize (a vector, a color, a
        // preloaded resource). Godot would print the value; naming the kind is the honest stand-in,
        // and no message that reaches here can be pinned against the engine anyway.
        FoldedValue::Opaque(vt, None) => variant_type_name(*vt).to_owned(),
    }
}

/// `stringify_variant_clean` (`core/variant/variant.cpp:1562`) — the rendering a value gets when it
/// sits inside a collection: a string is escaped and quoted, and a `StringName` / `NodePath` also
/// carries its sigil, so the three stay distinguishable from each other and from a bare token.
fn stringify_clean(value: &FoldedValue, recursion: usize) -> String {
    let s = stringify_at(value, recursion);
    match value {
        FoldedValue::String(_) => format!("\"{}\"", c_escape(&s)),
        FoldedValue::StringName(_) => format!("&\"{}\"", c_escape(&s)),
        FoldedValue::NodePath(_) => format!("^\"{}\"", c_escape(&s)),
        _ => s,
    }
}

/// `String::c_escape` (`core/string/ustring.cpp`) — the escapes Godot writes, in its order.
fn c_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\x07' => out.push_str("\\a"),
            '\x08' => out.push_str("\\b"),
            '\x0c' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x0b' => out.push_str("\\v"),
            '\'' => out.push_str("\\'"),
            '"' => out.push_str("\\\""),
            other => out.push(other),
        }
    }
    out
}

/// `String::num_real(p_num, true)` (`core/string/ustring.cpp`), the float rendering
/// `Variant::stringify` uses. An integral value keeps a `.0` tail; everything else is printed to 14
/// decimals, minus the magnitude's own digits above 10, with trailing zeroes destroyed except the
/// one after the period.
fn num_real(f: f64) -> String {
    if f.is_nan() {
        return "nan".to_owned();
    }
    if f.is_infinite() {
        return if f.is_sign_negative() { "-inf" } else { "inf" }.to_owned();
    }
    // `p_num == (double)(int64_t)p_num` — true for `-0.0` too, which is why it renders `0.0`.
    if f == (f as i64) as f64 {
        return format!("{}.0", f as i64);
    }

    let mut decimals: i32 = 14;
    let abs = f.abs();
    if abs > 10.0 {
        decimals -= abs.log10().floor() as i32;
    }
    // `String::num` builds a printf format from `p_decimals`. A NEGATIVE count writes `"%lf"` with
    // no precision, which is C's default of 6 — not zero, and not a clamp. A magnitude past 1e20
    // lands here, and getting it wrong drops the `.0` tail the engine prints.
    const MAX_DECIMALS: i32 = 32;
    let decimals = if decimals < 0 {
        6
    } else {
        decimals.min(MAX_DECIMALS) as usize
    };
    let printed = format!("{f:.decimals$}");

    // "Destroy trailing zeroes, except one after period."
    if printed.contains('.') {
        let trimmed = printed.trim_end_matches('0');
        if trimmed.ends_with('.') {
            return format!("{trimmed}0");
        }
        return trimmed.to_owned();
    }
    printed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn arr(items: Vec<FoldedValue>) -> FoldedValue {
        FoldedValue::Array(Arc::new(items))
    }

    fn dict(pairs: Vec<(FoldedValue, FoldedValue)>) -> FoldedValue {
        FoldedValue::Dictionary(Arc::new(pairs))
    }

    /// Every row pinned against `Godot_v4.7.2-stable --headless --check-only`, read out of the
    /// `Cannot get index "zz" from "%s".` message the constant-subscript miss produces.
    #[test]
    fn a_collection_renders_the_way_godot_prints_it() {
        assert_eq!(
            stringify(&arr(vec![FoldedValue::Int(1), FoldedValue::Int(2)])),
            "[1, 2]"
        );
        assert_eq!(stringify(&arr(Vec::new())), "[]");
        // The two spaces are Godot's leading-and-trailing space with nothing between them.
        assert_eq!(stringify(&dict(Vec::new())), "{  }");
        assert_eq!(
            stringify(&dict(vec![(
                FoldedValue::String("a".into()),
                FoldedValue::Int(1)
            )])),
            r#"{ "a": 1 }"#
        );
        assert_eq!(
            stringify(&dict(vec![
                (FoldedValue::String("a".into()), FoldedValue::Int(1)),
                (
                    FoldedValue::String("b".into()),
                    arr(vec![FoldedValue::Int(1), FoldedValue::Float(2.5)])
                ),
                (
                    FoldedValue::String("c".into()),
                    dict(vec![(
                        FoldedValue::String("d".into()),
                        FoldedValue::StringName("n".into())
                    )])
                ),
            ])),
            r#"{ "a": 1, "b": [1, 2.5], "c": { "d": &"n" } }"#
        );
        assert_eq!(
            stringify(&arr(vec![
                FoldedValue::Float(1.0),
                FoldedValue::Float(0.5),
                FoldedValue::Float(-3.25),
                FoldedValue::String("q\"x".into()),
                FoldedValue::StringName("sn".into()),
                FoldedValue::NodePath("np".into()),
                FoldedValue::Bool(true),
                FoldedValue::Nil,
            ])),
            r#"[1.0, 0.5, -3.25, "q\"x", &"sn", ^"np", true, <null>]"#
        );
    }

    /// A string-like prints bare at the top level and quoted inside a collection — the whole reason
    /// `stringify_variant_clean` exists as a separate function upstream.
    #[test]
    fn a_string_is_bare_at_the_top_level_and_quoted_inside() {
        assert_eq!(stringify(&FoldedValue::String("a".into())), "a");
        assert_eq!(stringify(&FoldedValue::StringName("a".into())), "a");
        assert_eq!(stringify(&FoldedValue::NodePath("a".into())), "a");
        assert_eq!(
            stringify(&arr(vec![FoldedValue::String("a".into())])),
            r#"["a"]"#
        );
    }

    /// `num_real`'s own rows, pinned the same way.
    #[test]
    fn a_float_renders_through_num_real() {
        let f = |x: f64| stringify(&FoldedValue::Float(x));
        assert_eq!(f(1e20), "100000000000000000000.0");
        assert_eq!(f(1e-8), "0.00000001");
        assert_eq!(f(0.1), "0.1");
        assert_eq!(f(100.125), "100.125");
        assert_eq!(f(-0.0), "0.0");
        assert_eq!(f(1234567890123.5), "1234567890123.5");
        assert_eq!(f(f64::NAN), "nan");
        assert_eq!(f(f64::INFINITY), "inf");
        assert_eq!(f(f64::NEG_INFINITY), "-inf");
    }

    /// Godot stops at `MAX_RECURSION` rather than overflowing the stack. gdls cannot build a cyclic
    /// fold, but a deeply nested literal reaches the same guard.
    #[test]
    fn deep_nesting_stops_at_the_recursion_guard() {
        let mut v = FoldedValue::Int(1);
        for _ in 0..(MAX_RECURSION + 5) {
            v = arr(vec![v]);
        }
        assert!(stringify(&v).contains("[...]"));
    }
}
