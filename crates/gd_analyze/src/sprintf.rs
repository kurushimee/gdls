//! Validate-only port of `String::sprintf` (core/string/ustring.cpp:5182).
//!
//! `"fmt" % value` folds through `OperatorEvaluatorStringFormat::do_mod` (core/variant/variant_op.h:724-836),
//! which wraps the right operand in a one-element `Array` and hands it to `String::sprintf`. When
//! sprintf fails it returns its error text *as the result string*, so `reduce_binary_op` reports
//! `<sprintf message> in operator %.` and leaves the node constant with that text as its value
//! (gdscript_analyzer.cpp:3149, :3163). This module reproduces sprintf's scanner and every one of
//! its twelve error returns, and nothing else: the padding, sign, and number-rendering bodies are
//! dropped, because no error in sprintf depends on the formatted output.
//!
//! Every specifier is covered. What is scoped is the *value*: [`validate`] runs only over a
//! [`FoldedValue`] gdls has actually materialized. A value gdls folds opaquely — a builtin constant
//! like `Vector2(1, 2)` — aborts the whole check, so `"%d" % Vector2(1, 2)` is a deliberate
//! under-report where Godot errors. Reporting there would mean guessing at a value we do not hold.

use gd_syntax::Dialect;

use crate::foldtable::FoldedValue;

/// The `Variant` a materialized [`FoldedValue`] stands for, narrowed to what sprintf inspects.
///
/// sprintf asks a value three questions — `is_num()` (variant.h:391, `INT` or `FLOAT` only),
/// `get_type()` against `STRING` and the six vector types, and its integer value for `%c` and `*`.
/// Nothing else about a value reaches an error path.
#[derive(Clone, Copy, Debug)]
enum Arg {
    Nil,
    Bool,
    Int(i64),
    Float(f64),
    /// A `Variant::STRING`, with its length in UTF-32 units — the one thing `%c` asks of it.
    Str(usize),
    /// `&"x"` and `^"x"`: not `is_num()`, and not `Variant::STRING`, so `%c` rejects them.
    NotStr,
}

impl Arg {
    /// `Variant::is_num` (variant.h:391).
    fn is_num(self) -> bool {
        matches!(self, Arg::Int(_) | Arg::Float(_))
    }

    /// The value read through a C `int`, as `int value = values[index]` does for `%c` and `*`.
    /// A float truncates toward zero first, then the 64-bit result narrows, which is why
    /// `"%c" % 4294967361` passes the range checks as `65` rather than failing them.
    fn as_c_int(self) -> Option<i32> {
        match self {
            Arg::Int(v) => Some(v as i32),
            Arg::Float(v) => Some(v as i64 as i32),
            _ => None,
        }
    }
}

/// The right operand of `%` as sprintf sees it, or `None` when gdls holds no value for it.
fn as_arg(value: &FoldedValue) -> Option<Arg> {
    match value {
        FoldedValue::Nil => Some(Arg::Nil),
        FoldedValue::Bool(_) => Some(Arg::Bool),
        FoldedValue::Int(v) => Some(Arg::Int(*v)),
        FoldedValue::Float(v) => Some(Arg::Float(*v)),
        FoldedValue::String(s) => Some(Arg::Str(s.chars().count())),
        FoldedValue::StringName(_) | FoldedValue::NodePath(_) => Some(Arg::NotStr),
        // Value unknown (`Opaque`), or never reduced at this site at all (`Array`/`Dictionary` —
        // Godot's `reduce_array` sets no `reduced_value`, so upstream is silent here too).
        FoldedValue::Opaque(..) | FoldedValue::Array(_) | FoldedValue::Dictionary(_) => None,
    }
}

/// Whether the left operand of `%` is the `String`/`StringName` that dispatches to
/// `OperatorEvaluatorStringFormat`, and its text.
pub(crate) fn format_string(value: &FoldedValue) -> Option<&str> {
    match value {
        FoldedValue::String(s) | FoldedValue::StringName(s) => Some(s),
        _ => None,
    }
}

/// Walk `format` against the one-element value span the `%` operator builds, returning the exact
/// message `String::sprintf` would return, or `None` if the format is valid or gdls cannot see the
/// value well enough to judge.
pub(crate) fn validate(
    format: &str,
    value: &FoldedValue,
    dialect: Dialect,
) -> Option<&'static str> {
    let arg = as_arg(value)?;

    let mut in_format = false;
    let mut used = false;
    let mut value_index: u64 = 0;
    let mut selected_index: i32 = -1;
    let mut min_chars: i32 = 0;
    let mut in_decimals = false;

    // The span is always one element long: every `OperatorEvaluatorStringFormat` specialization
    // except the `Array` one wraps a single value, and an `Array` right operand never reaches here
    // (it has no fold). `Nil` is wrapped too — `do_mod`'s `<S, void>` form (variant_op.h:727).
    const VALUES_SIZE: u64 = 1;

    for c in format.chars() {
        if !in_format {
            if c == '%' {
                in_format = true;
                min_chars = 0;
                in_decimals = false;
                selected_index = -1;
            }
            continue;
        }

        // The index this specifier reads. Every value-consuming case checks it against the span
        // before touching the value, and every one of them shares the same bounds message.
        let index = if selected_index >= 0 {
            selected_index as u64
        } else {
            value_index
        };
        if matches!(c, 'd' | 'o' | 'x' | 'X' | 'f' | 'v' | 's' | 'c' | '*') && index >= VALUES_SIZE
        {
            return Some("not enough arguments for format string");
        }

        match c {
            // `%%` is a literal percent and reads no value.
            '%' => {
                in_format = false;
                continue;
            }
            'd' | 'o' | 'x' | 'X' | 'f' => {
                if !arg.is_num() {
                    return Some("a number is required");
                }
                in_format = false;
            }
            'v' => {
                // No materialized fold is a vector, so the `default` arm always wins here.
                return Some("%v requires a vector type (Vector2/3/4/2i/3i/4i)");
            }
            's' => {
                // Any `Variant` stringifies, so `%s` has no type error.
                in_format = false;
            }
            'c' => {
                if arg.is_num() {
                    let v = arg.as_c_int().expect("invariant: is_num implies as_c_int");
                    if v < 0 {
                        return Some("unsigned integer is lower than minimum");
                    } else if (0xd800..=0xdfff).contains(&v) {
                        return Some("unsigned integer is invalid Unicode character");
                    } else if v > 0x10ffff {
                        return Some("unsigned integer is greater than maximum");
                    }
                } else if let Arg::Str(len) = arg {
                    if len != 1 {
                        return Some("%c requires number or single-character string");
                    }
                } else {
                    return Some("%c requires number or single-character string");
                }
                in_format = false;
            }
            // Rendering-only flags: left justify, show sign, treat as unsigned.
            '-' | '+' | 'u' => continue,
            '0'..='9' => {
                // A leading `0` is the zero-pad flag, not a width digit.
                if !in_decimals && !(c == '0' && min_chars == 0) {
                    let n = c as i32 - '0' as i32;
                    min_chars = min_chars.wrapping_mul(10).wrapping_add(n);
                }
                continue;
            }
            // DIALECT(4.7): ustring.cpp:5521 — `%<n>$` selects an argument by position. At 4.6 `$`
            // is not a case in the switch, so it falls through to the default and is rejected.
            '$' if dialect >= Dialect::Godot4_7 => {
                if min_chars > 0 {
                    selected_index = min_chars - 1;
                }
                min_chars = 0;
                continue;
            }
            '.' => {
                if in_decimals {
                    return Some("too many decimal points in format");
                }
                in_decimals = true;
                continue;
            }
            '*' => {
                // No materialized fold is a vector, so `is_num()` is the whole test.
                if !arg.is_num() {
                    return Some("* wants number or vector");
                }
                if !in_decimals {
                    min_chars = arg.as_c_int().expect("invariant: is_num implies as_c_int");
                }
            }
            _ => return Some("unsupported format character"),
        }

        // Shared tail of every value-consuming case: advance the cursor and mark the value used.
        if selected_index == -1 {
            value_index += 1;
        }
        used = true;
    }

    if in_format {
        return Some("incomplete format");
    }
    if !used {
        return Some("not all arguments converted during string formatting");
    }
    None
}
