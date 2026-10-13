//! #601 — `debug/gdscript/warnings/directory_rules`.
//!
//! Godot registers a default of `{"res://addons": Exclude}` (`modules/gdscript/gdscript.cpp`,
//! identical at `4.6.3-stable` and `4.7.2-stable`) and checks it in `push_warning` above the
//! level lookup, so a script under an excluded directory reports nothing at all — not even a
//! warning the project promoted to an error. Third-party addon code is not the user's to fix.
//!
//! gdls reported all of it. On Pixelorama that was 11 warnings and 1 error across 6 addon files
//! that the project's own editor shows none of.

use std::path::Path;

use gd_analyze::{analyze, NoCrossFile, Severity, StrictProfile, StrictSettings, WarnPolicy};
use gd_project::WarningConfig;
use gd_syntax::{parse, Dialect};
use gd_types::NativeDb;

fn native_db() -> NativeDb {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../gd_types/tests/fixtures/trimmed_api.json");
    NativeDb::load(path.to_str().expect("utf-8 path"))
        .unwrap_or_else(|e| panic!("load native DB fixture at {}: {e}", path.display()))
}

/// One integer division (a plain warning), one `min()` inference off a Variant (a warning Godot
/// promotes to an error by default), and one genuine type error that must survive either way.
const SRC: &str = concat!(
    "extends Node\n",
    "\n",
    "func f(fps: Variant) -> void:\n",
    "\tvar half := 7 / 2\n",
    "\tvar den := min(32767, fps)\n",
    "\tvar bad: int = \"text\"\n",
    "\tprint(half, den, bad)\n",
);

fn diagnose(project: &WarningConfig, strict: &StrictSettings, path: &str) -> (usize, usize) {
    let tree = parse(SRC).tree;
    let db = native_db();
    let policy = WarnPolicy::build(project, strict, Dialect::DEFAULT);
    let result = analyze(&tree, None, path, &db, &NoCrossFile, &policy);
    let errors = result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .count();
    let warnings = result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Warning)
        .count();
    (errors, warnings)
}

fn stock() -> StrictSettings {
    StrictSettings::default()
}

/// Every diagnostic's message, errors before warnings, so a change in what survives the exclusion
/// reads as the message that moved rather than as a count.
fn messages(project: &WarningConfig, strict: &StrictSettings, path: &str) -> Vec<String> {
    let tree = parse(SRC).tree;
    let db = native_db();
    let policy = WarnPolicy::build(project, strict, Dialect::DEFAULT);
    let result = analyze(&tree, None, path, &db, &NoCrossFile, &policy);
    let mut out: Vec<(bool, String)> = result
        .diagnostics
        .iter()
        .map(|d| (d.severity() == Severity::Warning, d.message().to_owned()))
        .collect();
    out.sort_by_key(|a| a.0);
    out.into_iter().map(|(_, m)| m).collect()
}

#[test]
fn a_script_under_addons_reports_nothing_but_errors() {
    let project = WarningConfig::default();

    // Project source: the Variant inference is an error-by-default warning, the bad assignment
    // raises the narrowing pair, and the integer division warns.
    assert_eq!(
        messages(&project, &stock(), "res://src/main.gd"),
        vec![
            "The variable type is being inferred from a Variant value, so it will be typed as \
             Variant.",
            r#"Cannot assign a value of type "String" as "int"."#,
            r#"Cannot assign a value of type String to variable "bad" with specified type int."#,
            "Integer division. Decimal part will be discarded.",
        ]
    );

    // The same file inside an addon keeps only the real errors. The promoted warning goes with
    // the plain one, because the exclusion is checked before the level is ever read.
    assert_eq!(
        messages(&project, &stock(), "res://addons/vendor/thing.gd"),
        vec![
            r#"Cannot assign a value of type "String" as "int"."#,
            r#"Cannot assign a value of type String to variable "bad" with specified type int."#,
        ]
    );
}

#[test]
fn a_nested_include_carves_an_exception_out_of_the_exclusion() {
    let text = concat!(
        "[debug]\n\n",
        "gdscript/warnings/directory_rules={\n",
        "\"res://addons\": 0,\n",
        "\"res://addons/mine\": 1\n",
        "}\n",
    );
    let project = gd_project::parse_project_godot(text).warnings;

    let (_, warnings) = diagnose(&project, &stock(), "res://addons/vendor/thing.gd");
    assert_eq!(warnings, 0, "the broad exclusion still applies");

    let (_, warnings) = diagnose(&project, &stock(), "res://addons/mine/thing.gd");
    assert!(warnings > 0, "the deeper include wins");
}

#[test]
fn the_legacy_boolean_still_switches_it_off() {
    let project =
        gd_project::parse_project_godot("[debug]\n\ngdscript/warnings/exclude_addons=false\n")
            .warnings;
    let (_, warnings) = diagnose(&project, &stock(), "res://addons/vendor/thing.gd");
    assert!(warnings > 0, "the project asked for addon warnings");
}

/// gdls's own strict profile is a "show me everything" mode: it already overrides a project-wide
/// `enable = false`, and it overrides the directory exclusions the same way. The default profile
/// is what has to match the engine.
#[test]
fn the_strict_profile_looks_inside_addons_anyway() {
    let project = WarningConfig::default();
    let strict = StrictSettings {
        profile: StrictProfile::Strict,
        ..Default::default()
    };
    let (_, warnings) = diagnose(&project, &strict, "res://addons/vendor/thing.gd");
    assert!(warnings > 0, "strict ignores the exclusion");

    let off = StrictSettings {
        profile: StrictProfile::Off,
        ..Default::default()
    };
    let (_, warnings) = diagnose(&project, &off, "res://src/main.gd");
    assert_eq!(warnings, 0, "the off profile still reports nothing");
}
