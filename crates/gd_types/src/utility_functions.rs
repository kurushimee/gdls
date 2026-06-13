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

/// Whether `name` is a Variant utility function — the DB-independent mirror of
/// `Variant::has_utility_function`. Used so a bare utility reference resolves to a constant
/// `Callable` under any [`crate::ApiProvenance`], including `Absent`.
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
