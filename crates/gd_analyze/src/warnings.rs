//! The GDScript warning set — a faithful port of `GDScriptWarning` (`gdscript_warning.{h,cpp}`).
//!
//! The whole class is `#ifdef DEBUG_ENABLED` in Godot; we always build it. Grepped at port time:
//! **48** codes — 45 active (`UnassignedVariable`=0 … `OnreadyWithExport`=44) plus 3 deprecated,
//! never-produced codes (45–47, behind `#ifndef DISABLE_DEPRECATED`). Default levels are 33 `Warn`,
//! 8 `Ignore`, 4 `Error`. Names, order, default levels, and every message template are reproduced
//! verbatim so diagnostics match Godot character-for-character.

/// `GDScriptWarning::WarnLevel` (`gdscript_warning.h:41`), in Godot's order (`Ignore`=0).
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarnLevel {
    Ignore = 0,
    Warn = 1,
    Error = 2,
}

/// `GDScriptWarning::Code` (`gdscript_warning.h:47`), in declaration order. `#[repr(u8)]` so the
/// discriminant indexes [`WARN_NAMES`] / [`DEFAULT_LEVELS`] — the same dense-table trick `TokenKind`
/// uses in `gd_syntax`.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WarningCode {
    UnassignedVariable = 0,
    UnassignedVariableOpAssign,
    UnusedVariable,
    UnusedLocalConstant,
    UnusedPrivateClassVariable,
    UnusedParameter,
    UnusedSignal,
    ShadowedVariable,
    ShadowedVariableBaseClass,
    ShadowedGlobalIdentifier,
    UnreachableCode,
    UnreachablePattern,
    StandaloneExpression,
    StandaloneTernary,
    IncompatibleTernary,
    UntypedDeclaration,
    InferredDeclaration,
    UnsafePropertyAccess,
    UnsafeMethodAccess,
    UnsafeCast,
    UnsafeCallArgument,
    UnsafeVoidReturn,
    ReturnValueDiscarded,
    StaticCalledOnInstance,
    MissingTool,
    RedundantStaticUnload,
    RedundantAwait,
    MissingAwait,
    AssertAlwaysTrue,
    AssertAlwaysFalse,
    IntegerDivision,
    NarrowingConversion,
    IntAsEnumWithoutCast,
    IntAsEnumWithoutMatch,
    EnumVariableWithoutDefault,
    EmptyFile,
    DeprecatedKeyword,
    ConfusableIdentifier,
    ConfusableLocalDeclaration,
    ConfusableLocalUsage,
    ConfusableCaptureReassignment,
    /// 4.7 (`gdscript_warning.h:90`). Absent from 4.6 — see [`WARNING_SINCE`].
    ConfusableTemporaryModification,
    InferenceOnVariant,
    NativeMethodOverride,
    GetNodeDefaultWithoutOnready,
    OnreadyWithExport,
    // Deprecated, gated behind `#ifndef DISABLE_DEPRECATED` in Godot and **never produced**
    // ("migrated from 3.x by mistake"). Kept for index parity with Godot's enum.
    PropertyUsedAsFunction,
    ConstantUsedAsFunction,
    FunctionUsedAsProperty,
}

/// Godot's `WARNING_MAX` at the newest supported tag (46 active + 3 deprecated). 4.6 has 48; the
/// extra code is gated by [`WARNING_SINCE`] rather than by a second table.
pub const WARNING_MAX: usize = gd_syntax::WARNING_COUNT;

/// Every code in discriminant order — lets [`code_from_name`] map an index back to a `WarningCode`
/// without an `unsafe` transmute.
pub const ALL: [WarningCode; WARNING_MAX] = {
    use WarningCode::*;
    [
        UnassignedVariable,
        UnassignedVariableOpAssign,
        UnusedVariable,
        UnusedLocalConstant,
        UnusedPrivateClassVariable,
        UnusedParameter,
        UnusedSignal,
        ShadowedVariable,
        ShadowedVariableBaseClass,
        ShadowedGlobalIdentifier,
        UnreachableCode,
        UnreachablePattern,
        StandaloneExpression,
        StandaloneTernary,
        IncompatibleTernary,
        UntypedDeclaration,
        InferredDeclaration,
        UnsafePropertyAccess,
        UnsafeMethodAccess,
        UnsafeCast,
        UnsafeCallArgument,
        UnsafeVoidReturn,
        ReturnValueDiscarded,
        StaticCalledOnInstance,
        MissingTool,
        RedundantStaticUnload,
        RedundantAwait,
        MissingAwait,
        AssertAlwaysTrue,
        AssertAlwaysFalse,
        IntegerDivision,
        NarrowingConversion,
        IntAsEnumWithoutCast,
        IntAsEnumWithoutMatch,
        EnumVariableWithoutDefault,
        EmptyFile,
        DeprecatedKeyword,
        ConfusableIdentifier,
        ConfusableLocalDeclaration,
        ConfusableLocalUsage,
        ConfusableCaptureReassignment,
        ConfusableTemporaryModification,
        InferenceOnVariant,
        NativeMethodOverride,
        GetNodeDefaultWithoutOnready,
        OnreadyWithExport,
        PropertyUsedAsFunction,
        ConstantUsedAsFunction,
        FunctionUsedAsProperty,
    ]
};

/// `PNAME` strings from `get_name_from_code` (`gdscript_warning.cpp:198`), in code order.
///
/// Projected from `gd_syntax::WARNINGS` rather than copied: Godot's parser reads the same table for
/// `@warning_ignore`, and gdls needs it in both crates. The asserts below pin this enum to it.
pub const WARN_NAMES: [&str; WARNING_MAX] = {
    let mut out = [""; WARNING_MAX];
    let mut i = 0;
    while i < WARNING_MAX {
        out[i] = gd_syntax::WARNINGS[i].0;
        i += 1;
    }
    out
};

/// The Godot release each code first appeared in, from the same table.
///
/// A code newer than the project's dialect can never fire: [`crate::warn_policy::WarnPolicy`]
/// resolves it to `Ignore`, and its emission site is behind a dialect guard.
pub const WARNING_SINCE: [gd_syntax::Dialect; WARNING_MAX] = {
    let mut out = [gd_syntax::Dialect::Godot4_6; WARNING_MAX];
    let mut i = 0;
    while i < WARNING_MAX {
        out[i] = gd_syntax::WARNINGS[i].1;
        i += 1;
    }
    out
};

/// Default level per code, from `default_warning_levels[]` (`gdscript_warning.h:105`): 33 `Warn`,
/// 8 `Ignore`, 4 `Error`. (Deprecated codes are `Warn` but never produced.)
pub const DEFAULT_LEVELS: [WarnLevel; WARNING_MAX] = {
    use WarnLevel::*;
    [
        Warn,   // UNASSIGNED_VARIABLE
        Warn,   // UNASSIGNED_VARIABLE_OP_ASSIGN
        Warn,   // UNUSED_VARIABLE
        Warn,   // UNUSED_LOCAL_CONSTANT
        Warn,   // UNUSED_PRIVATE_CLASS_VARIABLE
        Warn,   // UNUSED_PARAMETER
        Warn,   // UNUSED_SIGNAL
        Warn,   // SHADOWED_VARIABLE
        Warn,   // SHADOWED_VARIABLE_BASE_CLASS
        Warn,   // SHADOWED_GLOBAL_IDENTIFIER
        Warn,   // UNREACHABLE_CODE
        Warn,   // UNREACHABLE_PATTERN
        Warn,   // STANDALONE_EXPRESSION
        Warn,   // STANDALONE_TERNARY
        Warn,   // INCOMPATIBLE_TERNARY
        Ignore, // UNTYPED_DECLARATION
        Ignore, // INFERRED_DECLARATION
        Ignore, // UNSAFE_PROPERTY_ACCESS
        Ignore, // UNSAFE_METHOD_ACCESS
        Ignore, // UNSAFE_CAST
        Ignore, // UNSAFE_CALL_ARGUMENT
        Warn,   // UNSAFE_VOID_RETURN
        Ignore, // RETURN_VALUE_DISCARDED
        Warn,   // STATIC_CALLED_ON_INSTANCE
        Warn,   // MISSING_TOOL
        Warn,   // REDUNDANT_STATIC_UNLOAD
        Warn,   // REDUNDANT_AWAIT
        Ignore, // MISSING_AWAIT
        Warn,   // ASSERT_ALWAYS_TRUE
        Warn,   // ASSERT_ALWAYS_FALSE
        Warn,   // INTEGER_DIVISION
        Warn,   // NARROWING_CONVERSION
        Warn,   // INT_AS_ENUM_WITHOUT_CAST
        Warn,   // INT_AS_ENUM_WITHOUT_MATCH
        Warn,   // ENUM_VARIABLE_WITHOUT_DEFAULT
        Warn,   // EMPTY_FILE
        Warn,   // DEPRECATED_KEYWORD
        Warn,   // CONFUSABLE_IDENTIFIER
        Warn,   // CONFUSABLE_LOCAL_DECLARATION
        Warn,   // CONFUSABLE_LOCAL_USAGE
        Warn,   // CONFUSABLE_CAPTURE_REASSIGNMENT
        Warn,   // CONFUSABLE_TEMPORARY_MODIFICATION
        Error,  // INFERENCE_ON_VARIANT
        Error,  // NATIVE_METHOD_OVERRIDE
        Error,  // GET_NODE_DEFAULT_WITHOUT_ONREADY
        Error,  // ONREADY_WITH_EXPORT
        Warn,   // PROPERTY_USED_AS_FUNCTION (deprecated)
        Warn,   // CONSTANT_USED_AS_FUNCTION (deprecated)
        Warn,   // FUNCTION_USED_AS_PROPERTY (deprecated)
    ]
};

// Compile-time parity guards (mirror Godot's `static_assert`s).
const _: () = assert!(WarningCode::FunctionUsedAsProperty as usize == WARNING_MAX - 1);
const _: () = assert!(WARNING_SINCE.len() == WARNING_MAX);
const _: () = assert!(WARN_NAMES.len() == WARNING_MAX);
const _: () = assert!(DEFAULT_LEVELS.len() == WARNING_MAX);
const _: () = assert!(ALL.len() == WARNING_MAX);

impl WarningCode {
    /// Whether this code exists in `dialect`. A code Godot added in a later release must never be
    /// produced for a project pinned to an earlier one.
    #[must_use]
    pub fn exists_in(self, dialect: gd_syntax::Dialect) -> bool {
        WARNING_SINCE[self as usize] <= dialect
    }

    /// The release this code first appeared in.
    #[must_use]
    pub fn since(self) -> gd_syntax::Dialect {
        WARNING_SINCE[self as usize]
    }
}

/// Godot's `get_name_from_code`.
pub fn name_from_code(code: WarningCode) -> &'static str {
    WARN_NAMES[code as usize]
}

/// Godot's `get_default_value`.
pub fn default_level(code: WarningCode) -> WarnLevel {
    DEFAULT_LEVELS[code as usize]
}

/// Godot's `get_code_from_name` (case-sensitive on the upper-case `PNAME`s). Returns `None` for an
/// unknown name rather than Godot's `WARNING_MAX` sentinel.
pub fn code_from_name(name: &str) -> Option<WarningCode> {
    WARN_NAMES.iter().position(|&n| n == name).map(|i| ALL[i])
}

/// Build a warning's message, porting `GDScriptWarning::get_message` (`gdscript_warning.cpp:37`)
/// verbatim. `symbols` are the positional substitution values, in Godot's order. Reads of missing
/// symbols degrade to `""` (Godot's `CHECK_SYMBOLS` returns an empty string) — never a panic.
pub fn format_warning(code: WarningCode, symbols: &[String]) -> String {
    use WarningCode::*;
    let g = |i: usize| symbols.get(i).map(String::as_str).unwrap_or("");
    match code {
        UnassignedVariable => format!(r#"The variable "{0}" is used before being assigned a value."#, g(0)),
        UnassignedVariableOpAssign => format!(
            r#"The variable "{0}" is modified with the compound-assignment operator "{1}=" but was not previously initialized."#,
            g(0), g(1)
        ),
        UnusedVariable => format!(
            r#"The local variable "{v}" is declared but never used in the block. If this is intended, prefix it with an underscore: "_{v}"."#,
            v = g(0)
        ),
        UnusedLocalConstant => format!(
            r#"The local constant "{v}" is declared but never used in the block. If this is intended, prefix it with an underscore: "_{v}"."#,
            v = g(0)
        ),
        UnusedPrivateClassVariable => {
            format!(r#"The class variable "{0}" is declared but never used in the class."#, g(0))
        }
        UnusedParameter => format!(
            r#"The parameter "{p}" is never used in the function "{f}()". If this is intended, prefix it with an underscore: "_{p}"."#,
            p = g(1), f = g(0)
        ),
        UnusedSignal => {
            format!(r#"The signal "{0}" is declared but never explicitly used in the class."#, g(0))
        }
        ShadowedVariable => format!(
            r#"The local {0} "{1}" is shadowing an already-declared {2} at line {3} in the current class."#,
            g(0), g(1), g(2), g(3)
        ),
        ShadowedVariableBaseClass => {
            if symbols.len() > 4 {
                format!(
                    r#"The local {0} "{1}" is shadowing an already-declared {2} at line {3} in the base class "{4}"."#,
                    g(0), g(1), g(2), g(3), g(4)
                )
            } else {
                format!(
                    r#"The local {0} "{1}" is shadowing an already-declared {2} in the base class "{3}"."#,
                    g(0), g(1), g(2), g(3)
                )
            }
        }
        ShadowedGlobalIdentifier => {
            format!(r#"The {0} "{1}" has the same name as a {2}."#, g(0), g(1), g(2))
        }
        UnreachableCode => {
            format!(r#"Unreachable code (statement after return) in function "{0}()"."#, g(0))
        }
        UnreachablePattern => "Unreachable pattern (pattern after wildcard or bind).".to_owned(),
        StandaloneExpression => "Standalone expression (the line may have no effect).".to_owned(),
        StandaloneTernary => {
            "Standalone ternary operator (the return value is being discarded).".to_owned()
        }
        IncompatibleTernary => {
            "Values of the ternary operator are not mutually compatible.".to_owned()
        }
        UntypedDeclaration => {
            if g(0) == "Function" {
                format!(r#"{0} "{1}()" has no static return type."#, g(0), g(1))
            } else {
                format!(r#"{0} "{1}" has no static type."#, g(0), g(1))
            }
        }
        InferredDeclaration => {
            format!(r#"{0} "{1}" has an implicitly inferred static type."#, g(0), g(1))
        }
        UnsafePropertyAccess => format!(
            r#"The property "{0}" is not present on the inferred type "{1}" (but may be present on a subtype)."#,
            g(0), g(1)
        ),
        UnsafeMethodAccess => format!(
            r#"The method "{0}()" is not present on the inferred type "{1}" (but may be present on a subtype)."#,
            g(0), g(1)
        ),
        UnsafeCast => format!(r#"Casting "Variant" to "{0}" is unsafe."#, g(0)),
        UnsafeCallArgument => format!(
            r#"The argument {0} of the {1} "{2}()" requires the subtype "{3}" but the supertype "{4}" was provided."#,
            g(0), g(1), g(2), g(3), g(4)
        ),
        UnsafeVoidReturn => format!(
            r#"The method "{0}()" returns "void" but it's trying to return a call to "{1}()" that can't be ensured to also be "void"."#,
            g(0), g(1)
        ),
        ReturnValueDiscarded => {
            format!(r#"The function "{0}()" returns a value that will be discarded if not used."#, g(0))
        }
        StaticCalledOnInstance => format!(
            r#"The function "{f}()" is a static function but was called from an instance. Instead, it should be directly called from the type: "{c}.{f}()"."#,
            f = g(0), c = g(1)
        ),
        MissingTool => {
            r#"The base class script has the "@tool" annotation, but this script does not have it."#.to_owned()
        }
        RedundantStaticUnload => {
            r#"The "@static_unload" annotation is redundant because the file does not have a class with static variables."#.to_owned()
        }
        RedundantAwait => {
            r#""await" keyword is unnecessary because the expression isn't a coroutine nor a signal."#.to_owned()
        }
        MissingAwait => {
            r#""await" keyword might be desired because the expression is a coroutine."#.to_owned()
        }
        AssertAlwaysTrue => {
            "Assert statement is redundant because the expression is always true.".to_owned()
        }
        AssertAlwaysFalse => {
            "Assert statement will raise an error because the expression is always false.".to_owned()
        }
        IntegerDivision => "Integer division. Decimal part will be discarded.".to_owned(),
        NarrowingConversion => {
            "Narrowing conversion (float is converted to int and loses precision).".to_owned()
        }
        IntAsEnumWithoutCast => {
            r#"Integer used when an enum value is expected. If this is intended, cast the integer to the enum type using the "as" keyword."#.to_owned()
        }
        IntAsEnumWithoutMatch => format!(
            r#"Cannot {0} {1} as Enum "{2}": no enum member has matching value."#,
            g(0), g(1), g(2)
        ),
        EnumVariableWithoutDefault => format!(
            r#"The variable "{0}" has an enum type and does not set an explicit default value. The default will be set to "0"."#,
            g(0)
        ),
        EmptyFile => "Empty script file.".to_owned(),
        DeprecatedKeyword => format!(
            r#"The "{0}" keyword is deprecated and will be removed in a future release. Please replace it with "{1}"."#,
            g(0), g(1)
        ),
        ConfusableIdentifier => format!(
            r#"The identifier "{0}" has misleading characters and might be confused with something else."#,
            g(0)
        ),
        ConfusableLocalDeclaration => {
            format!(r#"The {0} "{1}" is declared below in the parent block."#, g(0), g(1))
        }
        ConfusableLocalUsage => {
            format!(r#"The identifier "{0}" will be shadowed below in the block."#, g(0))
        }
        ConfusableCaptureReassignment => format!(
            r#"Reassigning lambda capture does not modify the outer local variable "{0}"."#,
            g(0)
        ),
        // Two shapes, picked by symbol count exactly as Godot does (`gdscript_warning.cpp:157`):
        // three symbols when a non-`const` builtin method is the culprit, two for a plain
        // assignment chain. Note the argument order — the class name is `symbols[0]` but reads
        // last in both sentences.
        ConfusableTemporaryModification if symbols.len() > 2 => format!(
            r#"The built-in property "{1}" will not be modified as a result of calling the method "{2}()". Consider assigning the desired value to the property instead, or check the "{0}" class for a specialized API."#,
            g(0),
            g(1),
            g(2)
        ),
        ConfusableTemporaryModification => format!(
            r#"The built-in property "{1}" will not be modified in the assignment chain. Consider using a temporary variable and assigning it back instead, or check the "{0}" class for a specialized API."#,
            g(0),
            g(1)
        ),
        InferenceOnVariant => format!(
            "The {0} type is being inferred from a Variant value, so it will be typed as Variant.",
            g(0)
        ),
        NativeMethodOverride => format!(
            r#"The method "{0}()" overrides a method from native class "{1}". This won't be called by the engine and may not work as expected."#,
            g(0), g(1)
        ),
        GetNodeDefaultWithoutOnready => format!(
            r#"The default value uses "{0}" which won't return nodes in the scene tree before "_ready()" is called. Use the "@onready" annotation to solve this."#,
            g(0)
        ),
        OnreadyWithExport => {
            r#""@onready" will set the default value after "@export" takes effect and will override it."#.to_owned()
        }
        // Deprecated codes are never produced; Godot's get_message has no template for them.
        PropertyUsedAsFunction | ConstantUsedAsFunction | FunctionUsedAsProperty => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_round_trips_through_code() {
        for &code in &ALL {
            let name = name_from_code(code);
            assert_eq!(
                code_from_name(name),
                Some(code),
                "round-trip failed for {name}"
            );
        }
        assert_eq!(code_from_name("NOT_A_WARNING"), None);
    }

    #[test]
    fn level_distribution_matches_godot() {
        let count = |lvl: WarnLevel| DEFAULT_LEVELS.iter().filter(|&&l| l == lvl).count();
        // At the newest supported tag: 34 active + 3 deprecated default to Warn.
        assert_eq!(count(WarnLevel::Warn), 37);
        assert_eq!(count(WarnLevel::Ignore), 8);
        assert_eq!(count(WarnLevel::Error), 4);
        assert_eq!(
            count(WarnLevel::Warn) + count(WarnLevel::Ignore) + count(WarnLevel::Error),
            WARNING_MAX
        );
    }

    #[test]
    fn the_46_subset_keeps_its_own_distribution() {
        // 4.7 only added a code; it must not have moved any level 4.6 already had.
        let in_46 = |lvl: WarnLevel| {
            DEFAULT_LEVELS
                .iter()
                .zip(WARNING_SINCE)
                .filter(|(&l, since)| l == lvl && *since <= gd_syntax::Dialect::Godot4_6)
                .count()
        };
        assert_eq!(in_46(WarnLevel::Warn), 36, "33 active + 3 deprecated");
        assert_eq!(in_46(WarnLevel::Ignore), 8);
        assert_eq!(in_46(WarnLevel::Error), 4);
    }

    #[test]
    fn the_47_addition_is_the_only_new_code() {
        let new_in_47: Vec<&str> = WARNING_SINCE
            .iter()
            .zip(WARN_NAMES)
            .filter(|(&since, _)| since == gd_syntax::Dialect::Godot4_7)
            .map(|(_, name)| name)
            .collect();
        assert_eq!(new_in_47, ["CONFUSABLE_TEMPORARY_MODIFICATION"]);
        assert!(
            !WarningCode::ConfusableTemporaryModification.exists_in(gd_syntax::Dialect::Godot4_6)
        );
        assert!(
            WarningCode::ConfusableTemporaryModification.exists_in(gd_syntax::Dialect::Godot4_7)
        );
        // Everything else predates 4.7 and is available in both.
        for code in ALL {
            if code != WarningCode::ConfusableTemporaryModification {
                assert!(code.exists_in(gd_syntax::Dialect::Godot4_6), "{code:?}");
            }
        }
    }

    #[test]
    fn the_new_messages_match_godot_verbatim() {
        let sym = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            format_warning(
                WarningCode::ConfusableTemporaryModification,
                &sym(&["Node2D", "position"])
            ),
            r#"The built-in property "position" will not be modified in the assignment chain. Consider using a temporary variable and assigning it back instead, or check the "Node2D" class for a specialized API."#
        );
        assert_eq!(
            format_warning(
                WarningCode::ConfusableTemporaryModification,
                &sym(&["Node2D", "position", "normalize"])
            ),
            r#"The built-in property "position" will not be modified as a result of calling the method "normalize()". Consider assigning the desired value to the property instead, or check the "Node2D" class for a specialized API."#
        );
    }

    #[test]
    fn error_by_default_are_the_four() {
        for code in [
            WarningCode::InferenceOnVariant,
            WarningCode::NativeMethodOverride,
            WarningCode::GetNodeDefaultWithoutOnready,
            WarningCode::OnreadyWithExport,
        ] {
            assert_eq!(
                default_level(code),
                WarnLevel::Error,
                "{}",
                name_from_code(code)
            );
        }
    }

    #[test]
    fn messages_match_godot_templates() {
        assert_eq!(
            format_warning(WarningCode::UnassignedVariable, &["health".to_owned()]),
            r#"The variable "health" is used before being assigned a value."#
        );
        // The doubled substitution: variable name appears in both the message and the "_x" hint.
        assert_eq!(
            format_warning(WarningCode::UnusedVariable, &["speed".to_owned()]),
            r#"The local variable "speed" is declared but never used in the block. If this is intended, prefix it with an underscore: "_speed"."#
        );
        // Reordered symbols: UNUSED_PARAMETER takes [function, param].
        assert_eq!(
            format_warning(
                WarningCode::UnusedParameter,
                &["move".to_owned(), "delta".to_owned()]
            ),
            r#"The parameter "delta" is never used in the function "move()". If this is intended, prefix it with an underscore: "_delta"."#
        );
        // The "Function" special case in UNTYPED_DECLARATION.
        assert_eq!(
            format_warning(
                WarningCode::UntypedDeclaration,
                &["Function".to_owned(), "f".to_owned()]
            ),
            r#"Function "f()" has no static return type."#
        );
        assert_eq!(
            format_warning(
                WarningCode::UntypedDeclaration,
                &["Variable".to_owned(), "x".to_owned()]
            ),
            r#"Variable "x" has no static type."#
        );
        // No-symbol message.
        assert_eq!(
            format_warning(WarningCode::IntegerDivision, &[]),
            "Integer division. Decimal part will be discarded."
        );
    }

    #[test]
    fn missing_symbols_degrade_without_panic() {
        // The analyzer guarantees correct symbol counts; a short slice must still never panic.
        let _ = format_warning(WarningCode::UnsafeCallArgument, &[]);
        let _ = format_warning(WarningCode::ShadowedVariableBaseClass, &["var".to_owned()]);
    }
}
