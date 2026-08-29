//! The Godot feature-release whose GDScript frontend semantics are in force.
//!
//! gdls ports one frontend but serves projects pinned to different Godot feature releases. Only
//! the feature version matters: GDScript's frontend does not change across patch releases, so
//! `4.7.0` and `4.7.2` are the same dialect. The value is resolved once per workspace from
//! `project.godot`'s `application/config/features` entry and threaded into the lexer, the parser,
//! and the analyzer.
//!
//! # Porting discipline
//!
//! Godot itself has no version branching — each release is one behavior — so branching here is a
//! departure from the faithful-port rule and is fenced by three conventions that keep upstream
//! diffs re-applicable:
//!
//! 1. **Newest is primary.** The unguarded body of every ported function mirrors the newest
//!    supported tag. The *older* behavior is what gets wrapped. A 4.7 → 4.8 upstream diff then
//!    applies to the primary text the way it always has.
//! 2. **Ordered comparisons only** — `if self.dialect < Dialect::Godot4_7 { … }`, never `==`. A
//!    later release leaves existing guards untouched unless it changed the same site again.
//! 3. **One greppable marker** on every guard:
//!    `// DIALECT(4.7): gdscript_tokenizer.cpp:939 — a tab advances column by 1, not tab_size.`
//!    `grep -rn "DIALECT("` is the complete audit surface, the checklist for the next version
//!    bump, and the deletion list when a dialect is eventually retired.
//!
//! The dialect is carried as a struct field on `Lexer`, `Parser`, and the analyzer's context —
//! never as an extra parameter — so ported function signatures stay identical to Godot's.

/// A supported Godot feature release, ordered oldest to newest.
///
/// `Ord` is the point of the type: every guard asks "did this behavior exist yet", which is a
/// `<` or `>=` against a variant, so adding a newer release never disturbs existing guards.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Dialect {
    Godot4_6 = 0,
    Godot4_7 = 1,
}

impl Dialect {
    /// The newest release gdls ports. Moves on every version bump; the primary (unguarded) text of
    /// every ported function mirrors this tag.
    pub const NEWEST: Dialect = Dialect::Godot4_7;

    /// The oldest release gdls still serves.
    pub const OLDEST: Dialect = Dialect::Godot4_6;

    /// What an unspecified dialect means: the newest release gdls ports.
    ///
    /// This is what a bare `parse` / `analyze` call, a unit test, and a fuzz target get. It also
    /// matches what the server resolves for a project whose `project.godot` declares no version at
    /// all — which real projects never do, since Godot writes the feature list itself, so that path
    /// also logs a notice.
    pub const DEFAULT: Dialect = Dialect::NEWEST;

    /// The `major.minor` this dialect corresponds to — the shape found in `config/features`.
    #[must_use]
    pub fn version(self) -> (u32, u32) {
        match self {
            Dialect::Godot4_6 => (4, 6),
            Dialect::Godot4_7 => (4, 7),
        }
    }

    /// The `"major.minor"` tag, as written in `project.godot` and in user-facing messages.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Dialect::Godot4_6 => "4.6",
            Dialect::Godot4_7 => "4.7",
        }
    }

    /// Map a declared `major.minor` onto a supported dialect, clamping out of range.
    ///
    /// A version newer than anything ported clamps to [`Dialect::NEWEST`] and one older clamps to
    /// [`Dialect::OLDEST`], because serving a project with the nearest semantics beats refusing to
    /// serve it. Callers that need to tell the user which happened compare the result against the
    /// input themselves.
    #[must_use]
    pub fn from_version(major: u32, minor: u32) -> Dialect {
        match (major, minor) {
            (4, 6) => Dialect::Godot4_6,
            (4, 7) => Dialect::Godot4_7,
            v if v < (4, 6) => Dialect::OLDEST,
            _ => Dialect::NEWEST,
        }
    }

    /// Parse a `"major.minor"` tag. Rejects anything that is not exactly two numeric components,
    /// so a renderer name from the same `config/features` array can never be read as a version.
    #[must_use]
    pub fn parse_version(tag: &str) -> Option<(u32, u32)> {
        let (major, minor) = tag.split_once('.')?;
        if major.is_empty() || minor.is_empty() {
            return None;
        }
        Some((major.parse().ok()?, minor.parse().ok()?))
    }
}

impl Default for Dialect {
    fn default() -> Self {
        Dialect::DEFAULT
    }
}

impl std::fmt::Display for Dialect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_the_newest_port() {
        assert_eq!(Dialect::default(), Dialect::NEWEST);
    }

    #[test]
    fn ordering_is_oldest_to_newest() {
        assert!(Dialect::Godot4_6 < Dialect::Godot4_7);
        assert_eq!(Dialect::OLDEST, Dialect::Godot4_6);
        assert_eq!(Dialect::NEWEST, Dialect::Godot4_7);
    }

    #[test]
    fn exact_versions_map_to_their_dialect() {
        assert_eq!(Dialect::from_version(4, 6), Dialect::Godot4_6);
        assert_eq!(Dialect::from_version(4, 7), Dialect::Godot4_7);
    }

    #[test]
    fn out_of_range_versions_clamp() {
        assert_eq!(Dialect::from_version(4, 8), Dialect::NEWEST);
        assert_eq!(Dialect::from_version(5, 0), Dialect::NEWEST);
        assert_eq!(Dialect::from_version(4, 5), Dialect::OLDEST);
        assert_eq!(Dialect::from_version(3, 5), Dialect::OLDEST);
    }

    #[test]
    fn parse_version_accepts_two_numeric_components() {
        assert_eq!(Dialect::parse_version("4.7"), Some((4, 7)));
        assert_eq!(Dialect::parse_version("4.10"), Some((4, 10)));
    }

    #[test]
    fn parse_version_rejects_non_versions() {
        // The same `config/features` array carries renderer names and other tags.
        for tag in [
            "Forward Plus",
            "GL Compatibility",
            "Mobile",
            "4",
            "4.",
            ".7",
            "4.7.2",
            "",
            "v4.7",
        ] {
            assert_eq!(Dialect::parse_version(tag), None, "tag: {tag:?}");
        }
    }

    #[test]
    fn round_trips_through_version_and_str() {
        for d in [Dialect::Godot4_6, Dialect::Godot4_7] {
            let (major, minor) = d.version();
            assert_eq!(Dialect::from_version(major, minor), d);
            assert_eq!(Dialect::parse_version(d.as_str()), Some((major, minor)));
        }
    }
}
