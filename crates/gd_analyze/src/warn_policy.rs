//! The strict-mode policy layer (`docs/04-diagnostics-strict-mode.md`).
//!
//! A pure data computation — no analysis machinery — that resolves each warning's effective level
//! through Godot's precedence: **defaults → `project.godot` config → profile → fine-grained overrides
//! → inline `@warning_ignore`**. This module owns the first four; `@warning_ignore` (the last, and
//! always-wins) is per-scope and applied during the body pass via the analyzer's ignore table (WP-C+).
//!
//! `StrictProfile`/`StrictSettings` are defined here (not borrowed from `gd_server::config`) so the
//! analyzer stays free of the server crate; `gd_server` maps its parsed `initializationOptions` onto
//! these at the call site (WP-G).

use gd_project::{WarnLevel as ProjLevel, WarningConfig};

use crate::warnings::{code_from_name, WarnLevel, WarningCode, DEFAULT_LEVELS, WARNING_MAX};

/// The diagnostics profile (`docs/04` §3). Mirrors `gd_server::config::StrictProfile`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StrictProfile {
    /// Pure parity: honor only the project's own warning config.
    #[default]
    Godot,
    /// `godot`, then promote the typing family to errors ("always statically typed").
    Strict,
    /// Errors only; all warnings suppressed.
    Off,
}

/// The fine-grained, profile-independent overrides (`initializationOptions.strict`). Names are matched
/// case-insensitively against the warning `PNAME`s.
#[derive(Clone, Debug, Default)]
pub struct StrictSettings {
    pub profile: StrictProfile,
    pub enable_warnings: Vec<String>,
    pub disable_warnings: Vec<String>,
    pub error_warnings: Vec<String>,
}

/// The typing-related warnings the `strict` profile promotes to errors (`docs/04` §3): the
/// `UNTYPED`/`INFERRED` declarations and the `UNSAFE_*` family.
const STRICT_PROMOTED: [WarningCode; 6] = [
    WarningCode::UntypedDeclaration,
    WarningCode::InferredDeclaration,
    WarningCode::UnsafePropertyAccess,
    WarningCode::UnsafeMethodAccess,
    WarningCode::UnsafeCallArgument,
    WarningCode::UnsafeCast,
];

/// The resolved effective level for every warning code, indexed by discriminant.
#[derive(Clone, Debug)]
pub struct WarnPolicy {
    levels: [WarnLevel; WARNING_MAX],
}

impl WarnPolicy {
    /// Resolve every warning's level by applying the precedence chain (excluding the per-scope
    /// `@warning_ignore`, which the analyzer layers on at emit time).
    pub fn build(project: &WarningConfig, strict: &StrictSettings) -> Self {
        let mut levels = DEFAULT_LEVELS;

        // (1) project.godot. `enable = false` silences everything; otherwise apply per-name levels.
        if !project.enable {
            levels.fill(WarnLevel::Ignore);
        } else {
            for (name, &lvl) in &project.levels {
                if let Some(c) = code_from_name(&name.to_ascii_uppercase()) {
                    levels[c as usize] = from_project(lvl);
                }
            }
        }

        // (2) profile.
        match strict.profile {
            StrictProfile::Off => levels.fill(WarnLevel::Ignore),
            StrictProfile::Strict => {
                for &c in &STRICT_PROMOTED {
                    levels[c as usize] = WarnLevel::Error;
                }
            }
            StrictProfile::Godot => {}
        }

        // (3) fine-grained overrides, in Godot's order: enable, then disable, then error.
        // Unknown names are silently ignored — Godot semantics (`gdscript_warning.cpp`'s
        // `get_code_from_name` returns `WARNING_MAX` and callers `continue`). The server layer
        // (`gd_server::workspace::strict_settings`) logs each miss at `warn` so an operator can
        // see config typos in stderr; the analyzer itself stays dependency-free of `log`.
        for name in &strict.enable_warnings {
            if let Some(c) = code_from_name(&name.to_ascii_uppercase()) {
                if levels[c as usize] == WarnLevel::Ignore {
                    levels[c as usize] = WarnLevel::Warn;
                }
            }
        }
        for name in &strict.disable_warnings {
            if let Some(c) = code_from_name(&name.to_ascii_uppercase()) {
                levels[c as usize] = WarnLevel::Ignore;
            }
        }
        for name in &strict.error_warnings {
            if let Some(c) = code_from_name(&name.to_ascii_uppercase()) {
                levels[c as usize] = WarnLevel::Error;
            }
        }

        WarnPolicy { levels }
    }

    /// The effective level of a warning, before any `@warning_ignore` suppression.
    pub fn effective_level(&self, code: WarningCode) -> WarnLevel {
        self.levels[code as usize]
    }
}

fn from_project(level: ProjLevel) -> WarnLevel {
    match level {
        ProjLevel::Ignore => WarnLevel::Ignore,
        ProjLevel::Warn => WarnLevel::Warn,
        ProjLevel::Error => WarnLevel::Error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_project() -> WarningConfig {
        WarningConfig::default() // enable = true, no per-name levels
    }

    #[test]
    fn defaults_pass_through_under_godot_profile() {
        let p = WarnPolicy::build(&empty_project(), &StrictSettings::default());
        assert_eq!(
            p.effective_level(WarningCode::UnusedVariable),
            WarnLevel::Warn
        );
        assert_eq!(
            p.effective_level(WarningCode::UnsafeCast),
            WarnLevel::Ignore,
            "UNSAFE_* is Ignore by default"
        );
        assert_eq!(
            p.effective_level(WarningCode::InferenceOnVariant),
            WarnLevel::Error
        );
    }

    #[test]
    fn off_profile_silences_even_error_by_default() {
        let strict = StrictSettings {
            profile: StrictProfile::Off,
            ..Default::default()
        };
        let p = WarnPolicy::build(&empty_project(), &strict);
        assert_eq!(
            p.effective_level(WarningCode::UnusedVariable),
            WarnLevel::Ignore
        );
        assert_eq!(
            p.effective_level(WarningCode::InferenceOnVariant),
            WarnLevel::Ignore
        );
    }

    #[test]
    fn strict_profile_promotes_typing_family() {
        let strict = StrictSettings {
            profile: StrictProfile::Strict,
            ..Default::default()
        };
        let p = WarnPolicy::build(&empty_project(), &strict);
        for c in STRICT_PROMOTED {
            assert_eq!(
                p.effective_level(c),
                WarnLevel::Error,
                "{c:?} should be promoted"
            );
        }
        // A non-typing warning keeps its default.
        assert_eq!(
            p.effective_level(WarningCode::UnusedVariable),
            WarnLevel::Warn
        );
    }

    #[test]
    fn project_config_overrides_defaults() {
        let mut project = WarningConfig::default();
        project
            .levels
            .insert("unused_variable".to_owned(), ProjLevel::Ignore);
        project
            .levels
            .insert("integer_division".to_owned(), ProjLevel::Error);
        let p = WarnPolicy::build(&project, &StrictSettings::default());
        assert_eq!(
            p.effective_level(WarningCode::UnusedVariable),
            WarnLevel::Ignore
        );
        assert_eq!(
            p.effective_level(WarningCode::IntegerDivision),
            WarnLevel::Error
        );
    }

    #[test]
    fn fine_grained_overrides_win_over_profile() {
        // strict promotes UNSAFE_CAST to error; an explicit disable then turns it off again.
        let strict = StrictSettings {
            profile: StrictProfile::Strict,
            disable_warnings: vec!["unsafe_cast".to_owned()],
            error_warnings: vec!["narrowing_conversion".to_owned()],
            ..Default::default()
        };
        let p = WarnPolicy::build(&empty_project(), &strict);
        assert_eq!(
            p.effective_level(WarningCode::UnsafeCast),
            WarnLevel::Ignore
        );
        assert_eq!(
            p.effective_level(WarningCode::NarrowingConversion),
            WarnLevel::Error
        );
    }

    #[test]
    fn parses_real_project_godot_warnings() {
        // The real path: project.godot text → WarningConfig → policy.
        let pg = gd_project::parse_project_godot(
            "[debug]\ngdscript/warnings/unassigned_variable=0\ngdscript/warnings/untyped_declaration=2\n",
        );
        let p = WarnPolicy::build(&pg.warnings, &StrictSettings::default());
        assert_eq!(
            p.effective_level(WarningCode::UnassignedVariable),
            WarnLevel::Ignore
        );
        assert_eq!(
            p.effective_level(WarningCode::UntypedDeclaration),
            WarnLevel::Error
        );
    }
}
