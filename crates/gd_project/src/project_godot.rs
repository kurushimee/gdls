//! Tolerant parser for `project.godot`.
//!
//! `project.godot` is `ConfigFile` INI: `config_version` before any section, then `[section]`
//! blocks of `key=value`. The full setting key is `section/key` (Godot flattens it). We extract only
//! the handful of keys gdls needs — `config_version`, `application/run/main_scene`, `autoload/*`,
//! `debug/gdscript/warnings/*` — and discard the rest.
//!
//! The one real hazard (observed in a large real-world project): a value can span multiple physical lines when it
//! contains an `Object(...)` / array literal. A naïve line-splitter desyncs on the continuation. So
//! the reader merges physical lines into logical lines using bracket/quote depth before splitting on
//! `=`, even though those big values are then thrown away.

use gd_syntax::Dialect;
use rustc_hash::FxHashMap;

/// A GDScript warning level (`debug/gdscript/warnings/<name>` = `0`/`1`/`2`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarnLevel {
    Ignore,
    Warn,
    Error,
}

impl WarnLevel {
    fn from_u8(n: u8) -> Option<Self> {
        match n {
            0 => Some(Self::Ignore),
            1 => Some(Self::Warn),
            2 => Some(Self::Error),
            _ => None,
        }
    }
}

/// The project's GDScript warning configuration, as captured from `project.godot`. M3/M5 layer strict
/// profiles on top; M2 only records what the project itself declares.
#[derive(Clone, Debug)]
pub struct WarningConfig {
    pub enable: bool,
    /// Warning name (lowercase, as in the setting key) → level.
    pub levels: FxHashMap<String, WarnLevel>,
}

impl Default for WarningConfig {
    fn default() -> Self {
        // Godot's default: warnings on.
        Self {
            enable: true,
            levels: FxHashMap::default(),
        }
    }
}

/// What an autoload / `main_scene` value points at. `res://` paths are classified by extension;
/// `uid://` references are resolved later via the UID map; `.tscn` targets stay deferred until scene
/// typing (Phase 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResTarget {
    /// `res://….gd`.
    Script(String),
    /// `res://….tscn` / `.scn`.
    Scene(String),
    /// `uid://…` — resolve via the UID map.
    Uid(String),
    /// Anything else (or a `res://` resource we don't type).
    Unresolved(String),
}

/// Classify an unquoted, `*`-stripped target value.
pub fn classify(value: &str) -> ResTarget {
    if value.starts_with("uid://") {
        ResTarget::Uid(value.to_owned())
    } else if value.starts_with("res://") && value.ends_with(".gd") {
        ResTarget::Script(value.to_owned())
    } else if value.starts_with("res://") && (value.ends_with(".tscn") || value.ends_with(".scn")) {
        ResTarget::Scene(value.to_owned())
    } else {
        ResTarget::Unresolved(value.to_owned())
    }
}

/// An `[autoload]` entry. The leading `*` in the value means "register as a singleton".
#[derive(Clone, Debug)]
pub struct Autoload {
    pub name: String,
    pub target: ResTarget,
    pub is_singleton: bool,
}

/// The parsed `project.godot` (no filesystem resolution — see [`crate::model::ProjectModel`]).
/// `Default` (used when `project.godot` is absent) leans on [`WarningConfig`]'s own default
/// (warnings on).
#[derive(Clone, Debug, Default)]
pub struct ProjectGodot {
    pub config_version: u32,
    /// The Godot feature release the project declares, from `application/config/features` (e.g.
    /// `PackedStringArray("4.7", "Forward Plus")` → `Some((4, 7))`). `None` when the key is absent
    /// or carries no version-shaped entry.
    ///
    /// This is the only signal for the dialect. `config_version` is the *config file format*
    /// version (5 across all of 4.x) and says nothing about the feature release.
    pub declared_engine_version: Option<(u32, u32)>,
    pub main_scene: Option<ResTarget>,
    pub autoloads: Vec<Autoload>,
    pub warnings: WarningConfig,
}

/// Parse `project.godot` text. The thin wrapper over [`parse_with_confidence`] for callers that
/// don't need the corruption signal.
pub fn parse(text: &str) -> ProjectGodot {
    parse_with_confidence(text).0
}

/// WP-RD13: parse `project.godot` text AND report a `[0.0, 1.0]` confidence that the input was a
/// real config file rather than corrupt-but-parseable garbage. The tolerant parser silently skips
/// any line it can't make sense of, so a garbled or truncated file (a save caught mid-write,
/// binary content where text was expected) would otherwise parse to a near-default model that
/// looks "clean" — and reloading from it would wipe the project's real settings. Confidence is the
/// fraction of *meaningful* logical lines (non-blank, non-comment) that were structurally
/// recognized as a `[section]` header or a `key=value` pair; an empty/absent file is fully
/// confident (1.0). The reload path treats confidence below
/// [`crate::model::ProjectModel::CONFIDENCE_THRESHOLD`] as corrupt and keeps the prior model.
pub fn parse_with_confidence(text: &str) -> (ProjectGodot, f32) {
    let mut pg = ProjectGodot::default();
    let mut section = String::new();
    let mut meaningful = 0usize;
    let mut recognized = 0usize;
    for line in logical_lines(text) {
        let t = line.trim();
        if t.is_empty() || t.starts_with(';') {
            continue;
        }
        meaningful += 1;
        if let Some(name) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            recognized += 1;
            section = name.trim().to_owned();
            continue;
        }
        let Some((key, val)) = t.split_once('=') else {
            // A non-blank, non-comment line that is neither a section header nor a `key=value`
            // pair: structurally unrecognized (a corruption signal). Counted as meaningful but
            // not recognized so it drags confidence down.
            continue;
        };
        recognized += 1;
        let key = key.trim();
        let val = val.trim();
        let full = if section.is_empty() {
            key.to_owned()
        } else {
            format!("{section}/{key}")
        };

        if full == "config_version" {
            pg.config_version = val.parse().unwrap_or(0);
        } else if full == "application/run/main_scene" {
            pg.main_scene = Some(classify(&unquote(val)));
        } else if full == "application/config/features" {
            pg.declared_engine_version = parse_features_version(val);
        } else if let Some(name) = full.strip_prefix("autoload/") {
            let raw = unquote(val);
            let is_singleton = raw.starts_with('*');
            let target = classify(raw.trim_start_matches('*'));
            pg.autoloads.push(Autoload {
                name: name.to_owned(),
                target,
                is_singleton,
            });
        } else if let Some(w) = full.strip_prefix("debug/gdscript/warnings/") {
            if w == "enable" {
                pg.warnings.enable = val == "true";
            } else if let Some(level) = val.parse::<u8>().ok().and_then(WarnLevel::from_u8) {
                pg.warnings.levels.insert(w.to_owned(), level);
            }
        }
    }
    // An empty/absent config is a valid "all defaults" project, not corruption — full confidence.
    let confidence = if meaningful == 0 {
        1.0
    } else {
        recognized as f32 / meaningful as f32
    };
    (pg, confidence)
}

/// The byte range of the autoload NAME inside its `[autoload]` key line — the one place a
/// singleton's name is DECLARED (`Global="*res://global.gd"`), and therefore the edit an autoload
/// rename must make alongside the `.gd` occurrences (#157). Returns the range of the name TEXT only,
/// so the `*` singleton marker, the quoting and the target path are all preserved by a replacement.
///
/// Fail-closed, because the caller is a mutating consumer: `None` unless EXACTLY ONE key in the
/// `[autoload]` section names `name`. A key is recognized only in its unambiguous shape — a
/// physical line in the `[autoload]` section whose text before the first `=` is `name` (bare, or
/// double-quoted), and whose name is a plain identifier. A continuation line of a multi-line value
/// therefore cannot be mistaken for a key, and a duplicated entry refuses rather than editing one of
/// two.
#[must_use]
pub fn autoload_key_span(text: &str, name: &str) -> Option<std::ops::Range<usize>> {
    if name.is_empty() || !is_config_identifier(name) {
        return None;
    }
    let mut section = String::new();
    let mut found: Option<std::ops::Range<usize>> = None;
    let mut offset = 0usize;
    for line in text.split_inclusive('\n') {
        let start = offset;
        offset += line.len();
        let t = line.trim();
        if t.is_empty() || t.starts_with(';') {
            continue;
        }
        if let Some(sec) = t.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = sec.trim().to_owned();
            continue;
        }
        if section != "autoload" {
            continue;
        }
        let Some((key, _)) = line.split_once('=') else {
            continue;
        };
        let trimmed = key.trim();
        // The key's own extent inside the line, then the name's extent inside the key (which strips
        // a surrounding pair of quotes). Byte arithmetic on `line` — every offset is a real boundary
        // because `trim`/`strip_prefix` only cut at ASCII delimiters.
        let key_start = start + (trimmed.as_ptr() as usize - line.as_ptr() as usize);
        let (name_text, name_start) =
            match trimmed.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
                Some(inner) => (inner, key_start + 1),
                None => (trimmed, key_start),
            };
        if name_text != name || !is_config_identifier(name_text) {
            continue;
        }
        if found.is_some() {
            return None; // duplicated entry — refuse rather than edit one of two
        }
        found = Some(name_start..name_start + name_text.len());
    }
    found
}

/// True for a plain `[A-Za-z_][A-Za-z0-9_]*` name — what an autoload singleton key is allowed to be
/// (Godot registers it as a global identifier). Keeps [`autoload_key_span`] from matching a key that
/// only LOOKS like the name after trimming.
fn is_config_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Pull the engine feature version out of an `application/config/features` value.
///
/// The value is a `PackedStringArray("4.7", "Forward Plus")` literal whose entries mix the engine
/// version tag with renderer names and any custom feature tags the project defines. Only entries
/// shaped exactly `<major>.<minor>` count, so a renderer name can never be read as a version, and
/// the largest is taken if a malformed file somehow lists several.
///
/// Godot writes this key on every project save, so a real project always has it; a missing or
/// version-less value means a hand-edited or stripped file.
fn parse_features_version(value: &str) -> Option<(u32, u32)> {
    let trimmed = value.trim();
    // Take whatever sits between the first `(` and a trailing `)`, which covers the
    // `PackedStringArray(...)` Godot writes as well as a bare `(...)`. A value with no parens at
    // all is read as the list itself — the reader is deliberately forgiving everywhere else, and a
    // version is too cheap to lose to a syntax quibble.
    let inner = match (trimmed.find('('), trimmed.strip_suffix(')')) {
        (Some(open), Some(body)) => &body[open + 1..],
        _ => trimmed,
    };
    inner
        .split(',')
        .filter_map(|entry| Dialect::parse_version(&unquote(entry)))
        .max()
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    s.strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .unwrap_or(s)
        .to_owned()
}

/// Merge physical lines into logical lines, keeping a value that opens a bracket / quote together
/// with its continuation lines. Section headers, comments and blanks are atomic (never accumulated).
fn logical_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut depth = 0i32;
    let mut in_str = false;

    for line in text.lines() {
        if buf.is_empty() && depth == 0 && !in_str {
            let head = line.trim_start();
            if head.is_empty() || head.starts_with(';') || head.starts_with('[') {
                out.push(line.to_owned());
                continue;
            }
        }
        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str(line);
        scan(line, &mut depth, &mut in_str);
        if depth <= 0 && !in_str {
            depth = 0;
            out.push(std::mem::take(&mut buf));
        }
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

/// Update bracket depth / in-string state across one physical line.
fn scan(line: &str, depth: &mut i32, in_str: &mut bool) {
    let mut escaped = false;
    for c in line.chars() {
        if *in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                *in_str = false;
            }
        } else {
            match c {
                '"' => *in_str = true,
                '(' | '[' | '{' => *depth += 1,
                ')' | ']' | '}' => *depth -= 1,
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors a real large-project structure, including the multi-line Object(...) trap.
    #[test]
    fn sample_declares_its_engine_version() {
        // The `[application]` section's `config/features` carries the version tag alongside the
        // renderer name; only the version-shaped entry is picked up.
        assert_eq!(parse(SAMPLE).declared_engine_version, Some((4, 6)));
    }

    #[test]
    fn features_version_ignores_renderer_and_custom_tags() {
        for (value, want) in [
            (r#"PackedStringArray("4.7", "Forward Plus")"#, Some((4, 7))),
            (r#"PackedStringArray("Mobile", "4.6")"#, Some((4, 6))),
            (r#"PackedStringArray("4.7")"#, Some((4, 7))),
            // No version-shaped entry at all.
            (r#"PackedStringArray("Forward Plus")"#, None),
            (r#"PackedStringArray()"#, None),
            // A patch-qualified tag is not the shape Godot writes and is not a feature version.
            (r#"PackedStringArray("4.7.2")"#, None),
            // Unwrapped forms, since the reader is forgiving everywhere else.
            (r#"("4.7", "Mobile")"#, Some((4, 7))),
        ] {
            assert_eq!(parse_features_version(value), want, "value: {value}");
        }
    }

    #[test]
    fn features_version_survives_a_custom_tag_that_looks_numeric() {
        // A project may define its own feature tags; only two-component numerics count, and the
        // largest wins if a file somehow lists more than one version.
        assert_eq!(
            parse_features_version(r#"PackedStringArray("4.6", "4.7", "phase2")"#),
            Some((4, 7))
        );
    }

    #[test]
    fn missing_features_key_leaves_the_version_undeclared() {
        assert_eq!(
            parse("[application]\nconfig/name=\"X\"\n").declared_engine_version,
            None
        );
    }

    const SAMPLE: &str = r#"; comment
config_version=5

[Asset_Placer]

Shortcuts/Multi=Object(Shortcut,"resource_local_to_scene":false,"events":[Object(InputEventKey,"keycode":71,"script":null)
],"script":null)

[application]

config/name="Test Game"
run/main_scene="uid://abc123"
config/features=PackedStringArray("4.6", "Forward Plus")

[autoload]

StaticSignals="*res://src/static_signals.gd"
GShell="*res://assets/GShell.tscn"
Panku="*uid://cyftuo4syatlv"
PlainNode="res://nodes/plain.gd"

[debug]

gdscript/warnings/enable=true
gdscript/warnings/unassigned_variable=0
gdscript/warnings/untyped_declaration=2
gdscript/warnings/exclude_addons=true
"#;

    #[test]
    fn parses_scalars_and_main_scene() {
        let pg = parse(SAMPLE);
        assert_eq!(pg.config_version, 5);
        assert_eq!(pg.main_scene, Some(ResTarget::Uid("uid://abc123".into())));
    }

    #[test]
    fn multiline_value_does_not_desync_autoloads() {
        // If the multi-line shortcut value were mis-parsed, the [application]/[autoload] sections
        // after it would be lost. Assert they survived.
        let pg = parse(SAMPLE);
        assert_eq!(pg.autoloads.len(), 4);
    }

    #[test]
    fn autoload_targets_and_singleton_flag() {
        let pg = parse(SAMPLE);
        let by = |n: &str| pg.autoloads.iter().find(|a| a.name == n).unwrap();
        assert_eq!(
            by("StaticSignals").target,
            ResTarget::Script("res://src/static_signals.gd".into())
        );
        assert!(by("StaticSignals").is_singleton);
        assert_eq!(
            by("GShell").target,
            ResTarget::Scene("res://assets/GShell.tscn".into())
        );
        assert_eq!(
            by("Panku").target,
            ResTarget::Uid("uid://cyftuo4syatlv".into())
        );
        // No leading `*` ⇒ not a singleton.
        assert!(!by("PlainNode").is_singleton);
        assert_eq!(
            by("PlainNode").target,
            ResTarget::Script("res://nodes/plain.gd".into())
        );
    }

    #[test]
    fn warning_config() {
        let pg = parse(SAMPLE);
        assert!(pg.warnings.enable);
        assert_eq!(
            pg.warnings.levels.get("unassigned_variable"),
            Some(&WarnLevel::Ignore)
        );
        assert_eq!(
            pg.warnings.levels.get("untyped_declaration"),
            Some(&WarnLevel::Error)
        );
        // `exclude_addons=true` is not an int level ⇒ not recorded as one.
        assert!(!pg.warnings.levels.contains_key("exclude_addons"));
    }

    #[test]
    fn empty_input_is_default() {
        let pg = parse("");
        assert_eq!(pg.config_version, 0);
        assert!(pg.autoloads.is_empty());
        assert!(pg.warnings.enable);
    }

    #[test]
    fn confidence_high_for_valid_low_for_garbage() {
        // A real `project.godot` (every line a section or key=value) is fully recognized.
        assert!(
            parse_with_confidence(SAMPLE).1 > 0.9,
            "a real project.godot must be high-confidence"
        );
        // Empty / absent is a valid all-defaults project, not corruption.
        assert_eq!(parse_with_confidence("").1, 1.0);
        // Corrupt-but-parseable: lines with no `=` and no `[section]` header. The tolerant parser
        // accepts it as a near-default "clean" parse, but confidence flags it.
        let garbage = "asldkfj\nqwerty zxcv\n%%binary%%\nnot a config at all\n";
        assert!(
            parse_with_confidence(garbage).1 < 0.5,
            "garbled content must be low-confidence so the reload path preserves the prior model; \
             got {}",
            parse_with_confidence(garbage).1
        );
    }

    /// #157: the autoload key span is what a singleton rename rewrites, so it must cover the NAME
    /// only — the `*` singleton marker, the quoting and the path all survive a replacement.
    #[test]
    fn autoload_key_span_covers_only_the_name() {
        let text = "config_version=5\n\n[application]\n\nconfig/name=\"T\"\n\n\
                    [autoload]\n\nGlobal=\"*res://global.gd\"\nOther=\"res://other.gd\"\n";
        let span = autoload_key_span(text, "Global").expect("Global is declared");
        assert_eq!(&text[span.clone()], "Global");
        // Replacing exactly that range keeps the rest of the entry byte-for-byte.
        let mut renamed = text.to_owned();
        renamed.replace_range(span, "Settings");
        assert!(renamed.contains("Settings=\"*res://global.gd\""));
        assert!(renamed.contains("Other=\"res://other.gd\""));
    }

    /// A quoted key resolves to the name INSIDE the quotes (so the quotes survive), and a key
    /// outside the `[autoload]` section is never matched — `config/name` is not an autoload.
    #[test]
    fn autoload_key_span_handles_quoting_and_section_scope() {
        let text =
            "[application]\n\nconfig/name=\"Global\"\n\n[autoload]\n\n\"Global\"=\"*res://g.gd\"\n";
        let span = autoload_key_span(text, "Global").expect("Global is declared");
        assert_eq!(&text[span.clone()], "Global");
        assert_eq!(&text[span.start - 1..span.start], "\"");
        assert!(autoload_key_span(text, "name").is_none());
        assert!(autoload_key_span(text, "Missing").is_none());
    }

    /// Fail-closed for a mutating consumer: a duplicated entry refuses rather than editing one of
    /// two, and a non-identifier name is never matched.
    #[test]
    fn autoload_key_span_refuses_ambiguity() {
        let dup = "[autoload]\n\nGlobal=\"*res://a.gd\"\nGlobal=\"*res://b.gd\"\n";
        assert!(autoload_key_span(dup, "Global").is_none());
        assert!(autoload_key_span("[autoload]\n\nGlobal=\"*res://a.gd\"\n", "").is_none());
        assert!(autoload_key_span("[autoload]\n\n1Bad=\"*res://a.gd\"\n", "1Bad").is_none());
    }
}
