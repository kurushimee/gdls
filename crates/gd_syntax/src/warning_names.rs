//! The GDScript warning-code names, and the Godot release each first appeared in.
//!
//! Godot's parser applies `@warning_ignore*` while gdls keeps warning *policy* in `gd_analyze`, so
//! the names have to be reachable from both. They live here, at the root of the crate DAG, because
//! `gd_analyze` can depend on `gd_syntax` but not the reverse — mirroring Godot, whose parser
//! includes `gdscript_warning.h`. `gd_analyze::warnings` re-exports this table and pins it to its
//! own `WarningCode` enum with compile-time asserts, so the two can never drift.
//!
//! The order is `GDScriptWarning::Code`'s declaration order at the newest supported tag, which is
//! also the order `get_name_from_code` returns. A release that adds a code inserts it in place and
//! shifts every later ordinal — harmless, because nothing observable is keyed on the number:
//! `@warning_ignore`, `debug/gdscript/warnings/<name>`, the `.out` goldens, and the LSP diagnostic
//! code are all keyed on the name.

use crate::dialect::Dialect;

/// Godot's `WARNING_MAX` at the newest supported tag: 46 active plus 3 deprecated.
pub const WARNING_COUNT: usize = 49;

/// Every warning name in `GDScriptWarning::Code` order, paired with the release that introduced it.
///
/// A code is only valid in a project whose dialect is at least its release — `@warning_ignore` on a
/// name from a newer Godot is an error, exactly as that older Godot would report it.
pub const WARNINGS: [(&str, Dialect); WARNING_COUNT] = [
    ("UNASSIGNED_VARIABLE", Dialect::Godot4_6),
    ("UNASSIGNED_VARIABLE_OP_ASSIGN", Dialect::Godot4_6),
    ("UNUSED_VARIABLE", Dialect::Godot4_6),
    ("UNUSED_LOCAL_CONSTANT", Dialect::Godot4_6),
    ("UNUSED_PRIVATE_CLASS_VARIABLE", Dialect::Godot4_6),
    ("UNUSED_PARAMETER", Dialect::Godot4_6),
    ("UNUSED_SIGNAL", Dialect::Godot4_6),
    ("SHADOWED_VARIABLE", Dialect::Godot4_6),
    ("SHADOWED_VARIABLE_BASE_CLASS", Dialect::Godot4_6),
    ("SHADOWED_GLOBAL_IDENTIFIER", Dialect::Godot4_6),
    ("UNREACHABLE_CODE", Dialect::Godot4_6),
    ("UNREACHABLE_PATTERN", Dialect::Godot4_6),
    ("STANDALONE_EXPRESSION", Dialect::Godot4_6),
    ("STANDALONE_TERNARY", Dialect::Godot4_6),
    ("INCOMPATIBLE_TERNARY", Dialect::Godot4_6),
    ("UNTYPED_DECLARATION", Dialect::Godot4_6),
    ("INFERRED_DECLARATION", Dialect::Godot4_6),
    ("UNSAFE_PROPERTY_ACCESS", Dialect::Godot4_6),
    ("UNSAFE_METHOD_ACCESS", Dialect::Godot4_6),
    ("UNSAFE_CAST", Dialect::Godot4_6),
    ("UNSAFE_CALL_ARGUMENT", Dialect::Godot4_6),
    ("UNSAFE_VOID_RETURN", Dialect::Godot4_6),
    ("RETURN_VALUE_DISCARDED", Dialect::Godot4_6),
    ("STATIC_CALLED_ON_INSTANCE", Dialect::Godot4_6),
    ("MISSING_TOOL", Dialect::Godot4_6),
    ("REDUNDANT_STATIC_UNLOAD", Dialect::Godot4_6),
    ("REDUNDANT_AWAIT", Dialect::Godot4_6),
    ("MISSING_AWAIT", Dialect::Godot4_6),
    ("ASSERT_ALWAYS_TRUE", Dialect::Godot4_6),
    ("ASSERT_ALWAYS_FALSE", Dialect::Godot4_6),
    ("INTEGER_DIVISION", Dialect::Godot4_6),
    ("NARROWING_CONVERSION", Dialect::Godot4_6),
    ("INT_AS_ENUM_WITHOUT_CAST", Dialect::Godot4_6),
    ("INT_AS_ENUM_WITHOUT_MATCH", Dialect::Godot4_6),
    ("ENUM_VARIABLE_WITHOUT_DEFAULT", Dialect::Godot4_6),
    ("EMPTY_FILE", Dialect::Godot4_6),
    ("DEPRECATED_KEYWORD", Dialect::Godot4_6),
    ("CONFUSABLE_IDENTIFIER", Dialect::Godot4_6),
    ("CONFUSABLE_LOCAL_DECLARATION", Dialect::Godot4_6),
    ("CONFUSABLE_LOCAL_USAGE", Dialect::Godot4_6),
    ("CONFUSABLE_CAPTURE_REASSIGNMENT", Dialect::Godot4_6),
    ("CONFUSABLE_TEMPORARY_MODIFICATION", Dialect::Godot4_7),
    ("INFERENCE_ON_VARIANT", Dialect::Godot4_6),
    ("NATIVE_METHOD_OVERRIDE", Dialect::Godot4_6),
    ("GET_NODE_DEFAULT_WITHOUT_ONREADY", Dialect::Godot4_6),
    ("ONREADY_WITH_EXPORT", Dialect::Godot4_6),
    ("PROPERTY_USED_AS_FUNCTION", Dialect::Godot4_6),
    ("CONSTANT_USED_AS_FUNCTION", Dialect::Godot4_6),
    ("FUNCTION_USED_AS_PROPERTY", Dialect::Godot4_6),
];

/// The index of `upper_name` in [`WARNINGS`], or `None` if no such warning exists in any supported
/// release. Case-sensitive on the upper-case `PNAME`s, like Godot's `get_code_from_name`.
#[must_use]
pub fn warning_name_index(upper_name: &str) -> Option<usize> {
    WARNINGS.iter().position(|(name, _)| *name == upper_name)
}

/// Whether `upper_name` names a warning that exists in `dialect`.
#[must_use]
pub fn warning_name_is_valid(upper_name: &str, dialect: Dialect) -> bool {
    warning_name_index(upper_name).is_some_and(|i| WARNINGS[i].1 <= dialect)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique() {
        let mut seen: Vec<&str> = WARNINGS.iter().map(|(n, _)| *n).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(before, seen.len(), "duplicate warning name in the table");
    }

    #[test]
    fn a_newer_release_only_adds_codes() {
        // Nothing has ever been removed, so every 4.6 code must still be valid under 4.7.
        for (name, since) in WARNINGS {
            if since == Dialect::Godot4_6 {
                assert!(warning_name_is_valid(name, Dialect::Godot4_7), "{name}");
            }
        }
    }

    #[test]
    fn the_47_addition_is_rejected_under_46() {
        assert!(warning_name_is_valid(
            "CONFUSABLE_TEMPORARY_MODIFICATION",
            Dialect::Godot4_7
        ));
        assert!(!warning_name_is_valid(
            "CONFUSABLE_TEMPORARY_MODIFICATION",
            Dialect::Godot4_6
        ));
    }

    #[test]
    fn counts_match_each_release() {
        let n46 = WARNINGS
            .iter()
            .filter(|(_, s)| *s <= Dialect::Godot4_6)
            .count();
        let n47 = WARNINGS
            .iter()
            .filter(|(_, s)| *s <= Dialect::Godot4_7)
            .count();
        // 45 active + 3 deprecated at 4.6; one more active at 4.7.
        assert_eq!(n46, 48);
        assert_eq!(n47, 49);
    }

    #[test]
    fn unknown_names_are_rejected() {
        assert!(warning_name_index("NOT_A_WARNING").is_none());
        // Case-sensitive, like Godot's lookup.
        assert!(warning_name_index("unused_variable").is_none());
    }
}

#[cfg(test)]
mod parser_integration {
    use crate::dialect::Dialect;
    use crate::{parse_with_options, ParseOptions};

    fn diagnostics(src: &str, dialect: Dialect) -> Vec<String> {
        let options = ParseOptions {
            dialect,
            ..Default::default()
        };
        parse_with_options(src, &options)
            .diagnostics
            .into_iter()
            .map(|d| d.message)
            .collect()
    }

    #[test]
    fn ignoring_a_47_warning_is_an_error_under_46() {
        let src = "@warning_ignore(\"CONFUSABLE_TEMPORARY_MODIFICATION\")\nvar x = 1\n";
        assert_eq!(
            diagnostics(src, Dialect::Godot4_6),
            [r#"Invalid warning name: "CONFUSABLE_TEMPORARY_MODIFICATION"."#],
            "a 4.6 project has no such warning, exactly as Godot 4.6 reports it"
        );
        assert!(
            diagnostics(src, Dialect::Godot4_7).is_empty(),
            "the same annotation is valid under 4.7"
        );
    }

    #[test]
    fn a_shared_warning_name_is_accepted_by_both() {
        let src = "@warning_ignore(\"UNUSED_VARIABLE\")\nfunc f():\n\tvar x = 1\n";
        for dialect in [Dialect::Godot4_6, Dialect::Godot4_7] {
            assert!(diagnostics(src, dialect).is_empty(), "dialect {dialect}");
        }
    }

    #[test]
    fn an_unknown_name_is_rejected_by_both() {
        let src = "@warning_ignore(\"NOT_A_WARNING\")\nvar x = 1\n";
        for dialect in [Dialect::Godot4_6, Dialect::Godot4_7] {
            assert_eq!(
                diagnostics(src, dialect),
                [r#"Invalid warning name: "NOT_A_WARNING"."#],
                "dialect {dialect}"
            );
        }
    }

    #[test]
    fn the_region_form_is_gated_too() {
        // `@warning_ignore_start` validates names on the same table.
        let src = "@warning_ignore_start(\"CONFUSABLE_TEMPORARY_MODIFICATION\")\nvar x = 1\n\
                   @warning_ignore_restore(\"CONFUSABLE_TEMPORARY_MODIFICATION\")\n";
        assert!(
            diagnostics(src, Dialect::Godot4_6)
                .iter()
                .any(|d| d.contains("Invalid warning name")),
            "4.6 must reject the region form as well"
        );
        assert!(diagnostics(src, Dialect::Godot4_7).is_empty());
    }
}
