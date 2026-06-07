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
}
