//! The Variant utility-function registry.
//!
//! Godot's GDScript frontend treats a bare reference to a Variant utility (`print`, `floor`,
//! `abs`, …) as a first-class constant `Callable`, gated on `Variant::has_utility_function(name)`
//! (`core/variant/variant_utility.cpp`). That check is **compiled into the engine** — it does not
//! consult `extension_api.json`. Mirroring it here keeps utility resolution correct even when the
//! native DB carries no dump (`ApiProvenance::Absent`), where a DB-only lookup would miss and a
//! bare `print` would wrongly degrade to an undeclared-identifier error.
//!
//! The list is the `utility_functions` set of the 4.6.3-stable API surface (the
//! `register_utility_functions` table in `variant_utility.cpp`), in registration order. The
//! GDScript-only utilities (`len`, `range`, `load`, …) are a separate family resolved through the
//! analyzer's hard-coded GDScript-utility table, not this one.

/// Every Variant utility function name, in the engine's registration order. Equivalent to the
/// `utility_functions[].name` set of the stock `extension_api.json`; a DB ingest is verified
/// against this list so the two never drift.
pub const VARIANT_UTILITY_FUNCTIONS: &[&str] = &[
    "sin",
    "cos",
    "tan",
    "sinh",
    "cosh",
    "tanh",
    "asin",
    "acos",
    "atan",
    "atan2",
    "asinh",
    "acosh",
    "atanh",
    "sqrt",
    "fmod",
    "fposmod",
    "posmod",
    "floor",
    "floorf",
    "floori",
    "ceil",
    "ceilf",
    "ceili",
    "round",
    "roundf",
    "roundi",
    "abs",
    "absf",
    "absi",
    "sign",
    "signf",
    "signi",
    "snapped",
    "snappedf",
    "snappedi",
    "pow",
    "log",
    "exp",
    "is_nan",
    "is_inf",
    "is_equal_approx",
    "is_zero_approx",
    "is_finite",
    "ease",
    "step_decimals",
    "lerp",
    "lerpf",
    "cubic_interpolate",
    "cubic_interpolate_angle",
    "cubic_interpolate_in_time",
    "cubic_interpolate_angle_in_time",
    "bezier_interpolate",
    "bezier_derivative",
    "angle_difference",
    "lerp_angle",
    "inverse_lerp",
    "remap",
    "smoothstep",
    "move_toward",
    "rotate_toward",
    "deg_to_rad",
    "rad_to_deg",
    "linear_to_db",
    "db_to_linear",
    "wrap",
    "wrapi",
    "wrapf",
    "max",
    "maxi",
    "maxf",
    "min",
    "mini",
    "minf",
    "clamp",
    "clampi",
    "clampf",
    "nearest_po2",
    "pingpong",
    "randomize",
    "randi",
    "randf",
    "randi_range",
    "randf_range",
    "randfn",
    "seed",
    "rand_from_seed",
    "weakref",
    "typeof",
    "type_convert",
    "str",
    "error_string",
    "type_string",
    "print",
    "print_rich",
    "printerr",
    "printt",
    "prints",
    "printraw",
    "print_verbose",
    "push_error",
    "push_warning",
    "var_to_str",
    "str_to_var",
    "var_to_bytes",
    "bytes_to_var",
    "var_to_bytes_with_objects",
    "bytes_to_var_with_objects",
    "hash",
    "instance_from_id",
    "is_instance_id_valid",
    "is_instance_valid",
    "rid_allocate_id",
    "rid_from_int64",
    "is_same",
];

/// The `UTILITY_FUNC_TYPE_MATH` subset of [`VARIANT_UTILITY_FUNCTIONS`] — the utilities Godot is
/// willing to evaluate at compile time (`gdscript_analyzer.cpp:3509`, gated on
/// `Variant::get_utility_function_type(name) == Variant::UTILITY_FUNC_TYPE_MATH`). `absi(-1)` folds
/// and so is a legal `const` initializer; `str(1)` and `randi()` do not and are not.
///
/// The dump carries no category for a utility, so this is transcribed from the engine's own
/// registrations. Re-derive it at the tag with:
///
/// ```text
/// grep -oP 'FUNCBIND\w*\(\s*(\w+)\s*,[^;]*Variant::UTILITY_FUNC_TYPE_MATH' core/variant/variant_utility.cpp \
///   | grep -oP 'FUNCBIND\w*\(\s*\K\w+' | sort
/// ```
///
/// Identical at 4.6.3-stable and 4.7.2-stable, so no dialect guard is owed. Names are in the
/// engine's own sorted order.
pub const VARIANT_UTILITY_MATH_FUNCTIONS: &[&str] = &[
    "abs",
    "absf",
    "absi",
    "acos",
    "acosh",
    "angle_difference",
    "asin",
    "asinh",
    "atan",
    "atan2",
    "atanh",
    "bezier_derivative",
    "bezier_interpolate",
    "ceil",
    "ceilf",
    "ceili",
    "clamp",
    "clampf",
    "clampi",
    "cos",
    "cosh",
    "cubic_interpolate",
    "cubic_interpolate_angle",
    "cubic_interpolate_angle_in_time",
    "cubic_interpolate_in_time",
    "db_to_linear",
    "deg_to_rad",
    "ease",
    "exp",
    "floor",
    "floorf",
    "floori",
    "fmod",
    "fposmod",
    "inverse_lerp",
    "is_equal_approx",
    "is_finite",
    "is_inf",
    "is_nan",
    "is_zero_approx",
    "lerp",
    "lerp_angle",
    "lerpf",
    "linear_to_db",
    "log",
    "max",
    "maxf",
    "maxi",
    "min",
    "minf",
    "mini",
    "move_toward",
    "nearest_po2",
    "pingpong",
    "posmod",
    "pow",
    "rad_to_deg",
    "remap",
    "rotate_toward",
    "round",
    "roundf",
    "roundi",
    "sign",
    "signf",
    "signi",
    "sin",
    "sinh",
    "smoothstep",
    "snapped",
    "snappedf",
    "snappedi",
    "sqrt",
    "step_decimals",
    "tan",
    "tanh",
    "wrap",
    "wrapf",
    "wrapi",
];

/// Whether `name` is one of the math utilities Godot folds at compile time. See
/// [`VARIANT_UTILITY_MATH_FUNCTIONS`].
#[must_use]
pub fn is_variant_utility_math(name: &str) -> bool {
    VARIANT_UTILITY_MATH_FUNCTIONS.contains(&name)
}

/// Whether `name` is a Variant utility function — the DB-independent mirror of
/// `Variant::has_utility_function`. Used so a bare utility reference resolves to a constant
/// `Callable` under any [`crate::ApiProvenance`], including `Absent`.
///
/// A linear scan over 114 short `&str`s — deliberately simple, and only reached for an identifier
/// that every earlier name-resolution step already missed, so it is not on a hot path.
#[must_use]
pub fn is_variant_utility(name: &str) -> bool {
    VARIANT_UTILITY_FUNCTIONS.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_is_the_full_registry() {
        // The 4.6.3-stable Variant utility registry size. A mismatch means the list drifted from
        // the engine's `register_utility_functions` table.
        assert_eq!(VARIANT_UTILITY_FUNCTIONS.len(), 114);
    }

    #[test]
    fn math_subset_is_the_full_registry_and_lives_inside_the_whole() {
        // The `UTILITY_FUNC_TYPE_MATH` registration count, identical at both supported tags.
        assert_eq!(VARIANT_UTILITY_MATH_FUNCTIONS.len(), 78);
        for name in VARIANT_UTILITY_MATH_FUNCTIONS {
            assert!(is_variant_utility(name), "{name} must also be a utility");
        }
        for name in ["absi", "maxi", "lerp", "min", "snapped"] {
            assert!(is_variant_utility_math(name), "{name} folds in Godot");
        }
        // Not math, so not folded: I/O, randomness, conversion, reflection.
        for name in ["str", "randi", "print", "typeof", "weakref", "is_same"] {
            assert!(!is_variant_utility_math(name), "{name} does not fold");
        }
    }

    #[test]
    fn membership_covers_both_math_and_io_families() {
        for name in [
            "print", "floor", "abs", "typeof", "weakref", "clamp", "is_same",
        ] {
            assert!(is_variant_utility(name), "{name} must be a Variant utility");
        }
    }

    #[test]
    fn gdscript_only_and_unknown_names_are_not_variant_utilities() {
        // `len`/`range` are GDScript-only (a separate family); `nope` is nothing.
        for name in ["len", "range", "nope", ""] {
            assert!(
                !is_variant_utility(name),
                "{name} must not be a Variant utility"
            );
        }
    }
}
