//! Resolving which Godot dialect a project's scripts should be read as.
//!
//! One function owns the whole policy so there is exactly one place to read, log, and test it.
//! The inputs are the server's explicit override and whatever `project.godot` declared; the output
//! is a [`Dialect`] plus a [`DialectOrigin`] saying how it was reached, which the server turns into
//! a log line and, for the cases a user should know about, a `window/showMessage`.

use gd_syntax::Dialect;

/// How a resolved [`Dialect`] was arrived at. Every variant is logged; the three that mean gdls
/// guessed or corrected something also drive a one-time notice to the client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialectOrigin {
    /// `initializationOptions.dialect` named it explicitly. The user is in control; stay quiet.
    Override,
    /// `project.godot` declared a version gdls ports directly. The normal path; stay quiet.
    Declared,
    /// The project declared a version newer than anything gdls ports, so it was clamped down to
    /// [`Dialect::NEWEST`]. Worth saying — some of the project's syntax may not be understood.
    ClampedNewer,
    /// The project declared a version older than anything gdls ports, so it was clamped up to
    /// [`Dialect::OLDEST`]. Worth saying — diagnostics may not match that engine.
    ClampedOlder,
    /// A `project.godot` was read but declared no version, so the newest was assumed. Godot writes
    /// the feature tag on every project save, so this means a hand-edited or stripped file.
    DefaultedNewest,
    /// There is no readable `project.godot` at all, so the newest was assumed. This is the
    /// supported standalone-`.gd` mode, not a broken project — gdls already treats an absent
    /// project file as an expected degrade, so it stays quiet here too.
    NoProject,
}

impl DialectOrigin {
    /// Whether the release was pinned by actual evidence — the user's override, or what
    /// `project.godot` declared (clamped or not) — rather than assumed.
    ///
    /// The two assumed origins are the ones where a native dump's own header is the better
    /// witness: gdls guessed [`Dialect::NEWEST`] because nothing said otherwise, so a dump that
    /// disagrees is more likely right than the guess. Read by the server before it will replace
    /// or demote a dump over a release mismatch.
    #[must_use]
    pub fn is_evidenced(self) -> bool {
        matches!(
            self,
            DialectOrigin::Override
                | DialectOrigin::Declared
                | DialectOrigin::ClampedNewer
                | DialectOrigin::ClampedOlder
        )
    }

    /// Whether this origin warrants telling the user, rather than only the log.
    #[must_use]
    pub fn is_noteworthy(self) -> bool {
        matches!(
            self,
            DialectOrigin::ClampedNewer
                | DialectOrigin::ClampedOlder
                | DialectOrigin::DefaultedNewest
        )
    }
}

/// Decide which dialect to serve a project as.
///
/// Precedence is `override` → the declared `config/features` version → newest. A declared version
/// outside the ported range is clamped rather than refused, because serving a project with the
/// nearest semantics beats not serving it at all.
///
/// `project_file_read` says whether a `project.godot` was actually read. It only affects how loudly
/// an undeclared version is reported: a project file that exists but names no version was
/// hand-edited and is worth mentioning, while no project file at all is the ordinary
/// standalone-`.gd` case and stays quiet.
///
/// Defaulting to the newest (rather than the oldest) is deliberate: an undeclared version is not
/// something a working Godot project produces, so the case to optimize is a *new* project whose
/// config has not been written yet, not an old one.
#[must_use]
pub fn resolve_dialect(
    override_: Option<Dialect>,
    declared: Option<(u32, u32)>,
    project_file_read: bool,
) -> (Dialect, DialectOrigin) {
    if let Some(d) = override_ {
        return (d, DialectOrigin::Override);
    }
    let Some((major, minor)) = declared else {
        let origin = if project_file_read {
            DialectOrigin::DefaultedNewest
        } else {
            DialectOrigin::NoProject
        };
        return (Dialect::NEWEST, origin);
    };
    let resolved = Dialect::from_version(major, minor);
    let origin = if (major, minor) > Dialect::NEWEST.version() {
        DialectOrigin::ClampedNewer
    } else if (major, minor) < Dialect::OLDEST.version() {
        DialectOrigin::ClampedOlder
    } else {
        DialectOrigin::Declared
    };
    (resolved, origin)
}

/// A one-line, user-facing explanation of a noteworthy resolution, or `None` when the resolution
/// needs no explaining. `declared` is whatever the project asked for.
#[must_use]
pub fn dialect_notice(
    dialect: Dialect,
    origin: DialectOrigin,
    declared: Option<(u32, u32)>,
) -> Option<String> {
    let declared_tag = declared.map(|(major, minor)| format!("{major}.{minor}"));
    match origin {
        DialectOrigin::Override | DialectOrigin::Declared | DialectOrigin::NoProject => None,
        DialectOrigin::ClampedNewer => Some(format!(
            "project.godot declares Godot {}, which is newer than gdls supports — reading scripts \
             as Godot {dialect}. Newer syntax may be reported as an error.",
            declared_tag.unwrap_or_default()
        )),
        DialectOrigin::ClampedOlder => Some(format!(
            "project.godot declares Godot {}, which is older than gdls supports — reading scripts \
             as Godot {dialect}. Diagnostics may not match that engine.",
            declared_tag.unwrap_or_default()
        )),
        DialectOrigin::DefaultedNewest => Some(format!(
            "No Godot version found in project.godot (application → config/features), so scripts \
             are read as Godot {dialect}. Pin it with \
             config/features=PackedStringArray(\"{dialect}\") or the gdls `dialect` \
             initialization option."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_wins_over_everything() {
        let (d, o) = resolve_dialect(Some(Dialect::Godot4_6), Some((4, 7)), true);
        assert_eq!(d, Dialect::Godot4_6);
        assert_eq!(o, DialectOrigin::Override);
        assert!(dialect_notice(d, o, Some((4, 7))).is_none());
    }

    #[test]
    fn declared_supported_version_is_used_quietly() {
        for (v, want) in [((4, 6), Dialect::Godot4_6), ((4, 7), Dialect::Godot4_7)] {
            let (d, o) = resolve_dialect(None, Some(v), true);
            assert_eq!(d, want);
            assert_eq!(o, DialectOrigin::Declared);
            assert!(!o.is_noteworthy());
        }
    }

    #[test]
    fn newer_than_supported_clamps_down_and_says_so() {
        let (d, o) = resolve_dialect(None, Some((4, 9)), true);
        assert_eq!(d, Dialect::NEWEST);
        assert_eq!(o, DialectOrigin::ClampedNewer);
        let msg = dialect_notice(d, o, Some((4, 9))).expect("clamping up warrants a notice");
        assert!(msg.contains("4.9"), "{msg}");
    }

    #[test]
    fn older_than_supported_clamps_up_and_says_so() {
        let (d, o) = resolve_dialect(None, Some((3, 5)), true);
        assert_eq!(d, Dialect::OLDEST);
        assert_eq!(o, DialectOrigin::ClampedOlder);
        let msg = dialect_notice(d, o, Some((3, 5))).expect("clamping down warrants a notice");
        assert!(msg.contains("3.5"), "{msg}");
    }

    #[test]
    fn undeclared_defaults_to_newest_and_says_how_to_pin_it() {
        let (d, o) = resolve_dialect(None, None, true);
        assert_eq!(d, Dialect::NEWEST);
        assert_eq!(o, DialectOrigin::DefaultedNewest);
        let msg = dialect_notice(d, o, None).expect("a guess warrants a notice");
        assert!(msg.contains("config/features"), "{msg}");
        assert!(msg.contains(Dialect::NEWEST.as_str()), "{msg}");
    }

    #[test]
    fn no_project_file_defaults_to_newest_quietly() {
        // Editing a loose `.gd` with no project around it is a supported mode, not a broken
        // project — there is nothing for the user to fix, so there is nothing to say.
        let (d, o) = resolve_dialect(None, None, false);
        assert_eq!(d, Dialect::NEWEST);
        assert_eq!(o, DialectOrigin::NoProject);
        assert!(!o.is_noteworthy());
        assert!(dialect_notice(d, o, None).is_none());
    }
}
