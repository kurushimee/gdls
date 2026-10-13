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

/// What a `debug/gdscript/warnings/directory_rules` entry decides for the scripts under it
/// (`GDScriptParser::WarningDirectoryRule::Decision`). Stored in `project.godot` as `0` / `1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarnDirectoryDecision {
    Exclude,
    Include,
}

/// One directory rule. `directory` is `res://`-rooted, simplified, and carries a trailing slash,
/// matching the shape `GDScriptParser::update_project_settings` normalizes to before it compares
/// against a script's own path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WarnDirectoryRule {
    pub directory: String,
    pub decision: WarnDirectoryDecision,
}

/// Godot's registered default for `debug/gdscript/warnings/directory_rules`
/// (`modules/gdscript/gdscript.cpp`): third-party code under `res://addons` is not the user's to
/// fix, so none of its warnings are reported. Identical at `4.6.3-stable` and `4.7.2-stable`.
#[must_use]
pub fn default_warning_directory_rules() -> Vec<WarnDirectoryRule> {
    vec![WarnDirectoryRule {
        directory: "res://addons/".to_owned(),
        decision: WarnDirectoryDecision::Exclude,
    }]
}

/// The project's GDScript warning configuration, as captured from `project.godot`. M3/M5 layer strict
/// profiles on top; M2 only records what the project itself declares.
#[derive(Clone, Debug)]
pub struct WarningConfig {
    pub enable: bool,
    /// Warning name (lowercase, as in the setting key) → level.
    pub levels: FxHashMap<String, WarnLevel>,
    /// Which directories report warnings at all, deepest rule first. Defaults to Godot's own
    /// registered default rather than to "empty", so a project that never mentions the setting
    /// still excludes `res://addons/` the way the engine does.
    pub directory_rules: Vec<WarnDirectoryRule>,
}

impl Default for WarningConfig {
    fn default() -> Self {
        // Godot's default: warnings on, addons excluded.
        Self {
            enable: true,
            levels: FxHashMap::default(),
            directory_rules: default_warning_directory_rules(),
        }
    }
}

impl WarningConfig {
    /// Whether the script at `res_path` reports warnings at all — Godot's
    /// `evaluate_warning_directory_rules_for_script_path` (`gdscript_parser.cpp:328`). The first
    /// rule whose directory prefixes the path decides, and the list is already deepest-first, so a
    /// nested `Include` carves an exception out of a broader `Exclude`. A path that is not
    /// `res://`-rooted (an untitled buffer, a `.gd` outside the project) matches nothing and keeps
    /// its warnings.
    #[must_use]
    pub fn ignores_warnings_in(&self, res_path: &str) -> bool {
        self.directory_rules
            .iter()
            .find(|rule| res_path.starts_with(&rule.directory))
            .is_some_and(|rule| rule.decision == WarnDirectoryDecision::Exclude)
    }
}

/// Parse the `{ "res://dir": 0, … }` dictionary Godot writes for `directory_rules`, dropping the
/// entries it would drop: a key that is not `res://`-rooted and a decision outside the enum both
/// `ERR_CONTINUE` upstream. Returns `None` for a value that is not a dictionary at all, so the
/// caller keeps the default rather than silently turning every rule off.
fn parse_directory_rules(val: &str) -> Option<Vec<WarnDirectoryRule>> {
    let body = val.trim().strip_prefix('{')?.strip_suffix('}')?;
    let mut rules = Vec::new();
    for entry in split_outside_quotes(body, ',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let mut halves = split_outside_quotes(entry, ':');
        let (Some(key), Some(value)) = (halves.next(), halves.next()) else {
            continue;
        };
        let decision = match value.trim().parse::<u8>() {
            Ok(0) => WarnDirectoryDecision::Exclude,
            Ok(1) => WarnDirectoryDecision::Include,
            _ => continue,
        };
        let Some(directory) = simplify_res_dir(&unquote(key.trim())) else {
            continue;
        };
        rules.push(WarnDirectoryRule {
            decision,
            directory,
        });
    }
    Some(rules)
}

/// Split on `sep`, ignoring separators inside a double-quoted run. Godot's own reader is a full
/// `Variant` parser; a rules dictionary only ever holds string keys and integer values, so this
/// covers it without pulling one in.
fn split_outside_quotes(s: &str, sep: char) -> impl Iterator<Item = &str> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut in_quotes = false;
    for (i, c) in s.char_indices() {
        if c == '"' {
            in_quotes = !in_quotes;
        } else if c == sep && !in_quotes {
            parts.push(&s[start..i]);
            start = i + c.len_utf8();
        }
    }
    parts.push(&s[start..]);
    parts.into_iter()
}

/// A rule key normalized the way Godot normalizes one: `simplify_path`, then a mandatory trailing
/// slash. `None` for anything not `res://`-rooted, which upstream rejects outright.
fn simplify_res_dir(raw: &str) -> Option<String> {
    let rest = raw.trim().strip_prefix("res://")?;
    let mut segments: Vec<&str> = Vec::new();
    for seg in rest.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let mut out = String::from("res://");
    for seg in segments {
        out.push_str(seg);
        out.push('/');
    }
    Some(out)
}

/// Order the rules the way `update_project_settings` does before evaluating them: deepest
/// directory first, so `res://addons/mine` beats `res://addons`. Upstream sorts on the slash count
/// with an unstable sort; a stable one here keeps two same-depth rules in file order instead of an
/// arbitrary one.
fn sort_directory_rules(rules: &mut [WarnDirectoryRule]) {
    rules.sort_by_key(|r| std::cmp::Reverse(r.directory.matches('/').count()));
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
    let mut exclude_addons: Option<bool> = None;
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
            } else if w == "directory_rules" {
                if let Some(rules) = parse_directory_rules(val) {
                    pg.warnings.directory_rules = rules;
                }
            } else if w == "exclude_addons" {
                // Deprecated, and still migrated: upstream reads it, clears it, and writes the
                // result into `directory_rules` as `res://addons` — so it OVERRIDES whatever that
                // key said about addons. Applied after the scan, since either key may come first.
                exclude_addons = Some(val == "true");
            } else if let Some(level) = warn_level_value(w, val) {
                pg.warnings.levels.insert(w.to_owned(), level);
            }
        }
    }
    if let Some(exclude) = exclude_addons {
        let decision = if exclude {
            WarnDirectoryDecision::Exclude
        } else {
            WarnDirectoryDecision::Include
        };
        let rules = &mut pg.warnings.directory_rules;
        match rules.iter_mut().find(|r| r.directory == "res://addons/") {
            Some(rule) => rule.decision = decision,
            None => rules.push(WarnDirectoryRule {
                directory: "res://addons/".to_owned(),
                decision,
            }),
        }
    }
    sort_directory_rules(&mut pg.warnings.directory_rules);
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

/// The level of a `debug/gdscript/warnings/<name>` setting. Godot reads whatever Variant the file
/// stored and casts it to an int (`gdscript_parser.cpp:101`), so the older boolean form still in
/// the wild counts: `false` is `0` (ignore) and `true` is `1` (warn). #441 — dropping those left
/// a warning the project had turned off sitting at its default level.
///
/// The prefix also carries settings that are NOT warning codes and are genuinely boolean —
/// `exclude_addons`, `renamed_in_godot_4_hint` — so the boolean form is only read for a name the
/// warning table knows. Godot has the same guard for free: its loop walks the codes and reads each
/// one's own path, never the other way round.
fn warn_level_value(name: &str, val: &str) -> Option<WarnLevel> {
    match val {
        "false" | "true" => {
            gd_syntax::warning_names::warning_name_index(&name.to_ascii_uppercase()).map(|_| {
                if val == "true" {
                    WarnLevel::Warn
                } else {
                    WarnLevel::Ignore
                }
            })
        }
        _ => val.parse::<u8>().ok().and_then(WarnLevel::from_u8),
    }
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

    fn rules(text: &str) -> Vec<(String, WarnDirectoryDecision)> {
        parse(text)
            .warnings
            .directory_rules
            .into_iter()
            .map(|r| (r.directory, r.decision))
            .collect()
    }

    /// #601: a project that never mentions the setting still excludes `res://addons`, because
    /// that is what `GLOBAL_DEF` registers.
    #[test]
    fn addons_are_excluded_without_any_setting() {
        assert_eq!(
            rules("config_version=5\n"),
            vec![("res://addons/".to_owned(), WarnDirectoryDecision::Exclude)]
        );
        assert!(WarningConfig::default().ignores_warnings_in("res://addons/x/y.gd"));
        assert!(!WarningConfig::default().ignores_warnings_in("res://src/y.gd"));
        // A name that merely starts with the same letters is not inside the directory.
        assert!(!WarningConfig::default().ignores_warnings_in("res://addons_of_mine/y.gd"));
        // A buffer with no `res://` path at all matches nothing and keeps its warnings.
        assert!(!WarningConfig::default().ignores_warnings_in(""));
    }

    #[test]
    fn an_explicit_rule_list_replaces_the_default() {
        let text = "[debug]\n\ngdscript/warnings/directory_rules={\n\"res://vendor\": 0,\n\"res://addons\": 1\n}\n";
        assert_eq!(
            rules(text),
            vec![
                ("res://vendor/".to_owned(), WarnDirectoryDecision::Exclude),
                ("res://addons/".to_owned(), WarnDirectoryDecision::Include),
            ]
        );
        let cfg = parse(text).warnings;
        assert!(cfg.ignores_warnings_in("res://vendor/lib.gd"));
        assert!(!cfg.ignores_warnings_in("res://addons/mine/plugin.gd"));
    }

    /// Deepest directory first, so a nested `Include` carves an exception out of a broader
    /// `Exclude` no matter which order the file lists them in.
    #[test]
    fn the_deepest_rule_decides() {
        for text in [
            "[debug]\n\ngdscript/warnings/directory_rules={\n\"res://addons\": 0,\n\"res://addons/mine/deep\": 1\n}\n",
            "[debug]\n\ngdscript/warnings/directory_rules={\n\"res://addons/mine/deep\": 1,\n\"res://addons\": 0\n}\n",
        ] {
            let cfg = parse(text).warnings;
            assert!(!cfg.ignores_warnings_in("res://addons/mine/deep/a.gd"), "{text}");
            assert!(cfg.ignores_warnings_in("res://addons/mine/shallow.gd"), "{text}");
        }
    }

    /// The deprecated boolean is still migrated, and it overrides whatever the rule list said
    /// about `res://addons` — upstream writes it into the dictionary after reading that key.
    #[test]
    fn exclude_addons_migrates_and_wins() {
        let cfg = parse("[debug]\n\ngdscript/warnings/exclude_addons=false\n").warnings;
        assert!(!cfg.ignores_warnings_in("res://addons/x.gd"));

        let cfg = parse(
            "[debug]\n\ngdscript/warnings/directory_rules={\n\"res://addons\": 1\n}\ngdscript/warnings/exclude_addons=true\n",
        )
        .warnings;
        assert!(cfg.ignores_warnings_in("res://addons/x.gd"));

        // `exclude_addons` is not a warning code, so it never lands as a level (#441's guard).
        assert!(!parse("[debug]\n\ngdscript/warnings/exclude_addons=true\n")
            .warnings
            .levels
            .contains_key("exclude_addons"));
    }

    /// The entries upstream drops with `ERR_CONTINUE`: a key outside `res://` and a decision
    /// outside the enum. A value that is not a dictionary at all leaves the default standing,
    /// rather than silently switching every directory on.
    #[test]
    fn malformed_rule_entries_are_dropped_not_obeyed() {
        let cfg = parse(
            "[debug]\n\ngdscript/warnings/directory_rules={\n\"/etc\": 0,\n\"res://ok\": 0,\n\"res://bad\": 7\n}\n",
        )
        .warnings;
        assert_eq!(
            cfg.directory_rules,
            vec![WarnDirectoryRule {
                directory: "res://ok/".to_owned(),
                decision: WarnDirectoryDecision::Exclude,
            }]
        );

        assert_eq!(
            rules("[debug]\n\ngdscript/warnings/directory_rules=true\n"),
            vec![("res://addons/".to_owned(), WarnDirectoryDecision::Exclude)]
        );
    }

    /// A key is `simplify_path`-ed and given exactly one trailing slash before it is compared.
    #[test]
    fn rule_paths_are_normalized() {
        let cfg =
            parse("[debug]\n\ngdscript/warnings/directory_rules={\n\"res://a//b/./c/../\": 0\n}\n")
                .warnings;
        assert_eq!(
            cfg.directory_rules,
            vec![WarnDirectoryRule {
                directory: "res://a/b/".to_owned(),
                decision: WarnDirectoryDecision::Exclude,
            }]
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
        // `exclude_addons=true` is not a warning code ⇒ not recorded as a level, even though the
        // boolean form is otherwise read.
        assert!(!pg.warnings.levels.contains_key("exclude_addons"));
    }

    /// #441 — the older boolean form of a warning level. Godot casts whatever the file stored to
    /// an int (`gdscript_parser.cpp:101`), so `false` turns the warning off and `true` puts it at
    /// level 1. Dropping them left the warning at its default, which for `narrowing_conversion`
    /// means a project that had switched it off still saw it.
    #[test]
    fn a_boolean_warning_level_is_read_as_zero_or_one() {
        let pg = parse(
            "[debug]\n\n             gdscript/warnings/narrowing_conversion=false\n             gdscript/warnings/integer_division=true\n             gdscript/warnings/untyped_declaration=2\n",
        );
        assert_eq!(
            pg.warnings.levels.get("narrowing_conversion"),
            Some(&WarnLevel::Ignore)
        );
        assert_eq!(
            pg.warnings.levels.get("integer_division"),
            Some(&WarnLevel::Warn)
        );
        assert_eq!(
            pg.warnings.levels.get("untyped_declaration"),
            Some(&WarnLevel::Error)
        );
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
