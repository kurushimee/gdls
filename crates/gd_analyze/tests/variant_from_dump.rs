//! A type the dump declares is a HARD type, Variant included (#587).
//!
//! `type_from_property` stamps `ANNOTATED_EXPLICIT` before it looks at anything else
//! (`gdscript_analyzer.cpp:5841-5848`), so a native member the engine describes as returning
//! `Variant` carries a hard Variant, and `var x := abs(-1)` draws `INFERENCE_ON_VARIANT` — one of
//! the four warnings a stock Godot build reports as an error. gdls read that same declaration
//! through `DataType::variant()`, whose source is `Inferred`, and the warning's `is_hard_type()`
//! gate could never be reached along the path.
//!
//! The direction that must NOT change is gdls's own degrades. A soft Variant is also what every
//! silent miss falls back to — a trimmed dump, an unresolvable base — and inventing an
//! error-by-default warning on one of those would be the port claiming knowledge it does not have.
//! Only what the dump states is hard.
//!
//! Every row is pinned against Godot 4.7.2's own editor language server.

use std::path::Path;

use gd_analyze::{analyze_with_options, AnalyzeOptions, NoCrossFile, StrictSettings, WarnPolicy};
use gd_project::{FileId, WarningConfig};
use gd_syntax::{Dialect, ParseOptions};
use gd_types::NativeDb;

fn native_db() -> NativeDb {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../gd_types/tests/fixtures/trimmed_api.json");
    NativeDb::load(path.to_str().expect("utf-8 path"))
        .unwrap_or_else(|e| panic!("load native DB fixture at {}: {e}", path.display()))
}

/// Every diagnostic whose message is the inference-on-Variant one, by 1-based line. The rest of
/// what these scripts draw is the project's own warning config, not what this file is about.
fn inference_lines(body: &str) -> Vec<u32> {
    let src = format!("extends Node\n\n{body}");
    let tree = gd_syntax::parse_with_options(
        &src,
        &ParseOptions {
            dialect: Dialect::NEWEST,
            script_path: "",
        },
    )
    .tree;
    let db = native_db();
    let policy = WarnPolicy::build(
        &WarningConfig::default(),
        &StrictSettings::default(),
        Dialect::NEWEST,
    );
    analyze_with_options(
        &tree,
        Some(FileId::new(1)),
        "dump.gd",
        &db,
        &NoCrossFile,
        &policy,
        AnalyzeOptions {
            dialect: Dialect::NEWEST,
            ..Default::default()
        },
    )
    .diagnostics
    .iter()
    .filter(|d| {
        d.message() == "The variable type is being inferred from a Variant value, so it will be typed as Variant."
    })
    .filter_map(|d| d.line())
    .collect()
}

#[test]
fn a_variant_returning_utility_is_inferred_from_a_variant() {
    // `abs` is declared `"return_type": "Variant"`; `absi` is declared `int`. That single
    // difference is the whole split, and it holds even though both fold at compile time —
    // upstream sets the datatype from the DECLARATION after the fold, not from the folded value
    // (`gdscript_analyzer.cpp:3550`).
    assert_eq!(
        inference_lines("func f() -> void:\n\tvar a := abs(-1)\n\tprint(a)\n"),
        [4]
    );
    assert_eq!(
        inference_lines("func f() -> void:\n\tvar b := absi(-1)\n\tprint(b)\n"),
        Vec::<u32>::new()
    );
    // Not folded, and typed from a non-constant argument — same answer either way.
    assert_eq!(
        inference_lines("func f(g: int) -> void:\n\tvar h := abs(g)\n\tprint(h)\n"),
        [4]
    );
    assert_eq!(
        inference_lines("func f() -> void:\n\tvar i := clamp(1, 0, 2)\n\tprint(i)\n"),
        [4]
    );
}

#[test]
fn the_gdscript_only_family_answers_the_same_way() {
    // `convert` is registered `RETVAR` (gdscript_utility_functions.cpp:560), which reads back as
    // a hard Variant exactly like a dump-declared one — a second table, same rule. Every other
    // GDScript-only utility declares a concrete return and stays quiet.
    assert_eq!(
        inference_lines("func f() -> void:\n\tvar a := convert(1, TYPE_INT)\n\tprint(a)\n"),
        [4]
    );
    for call in [
        "len([1])",
        "range(3)",
        "load(\"res://x.gd\")",
        "is_instance_of(1, TYPE_INT)",
    ] {
        assert_eq!(
            inference_lines(&format!(
                "func f() -> void:\n\tvar a := {call}\n\tprint(a)\n"
            )),
            Vec::<u32>::new(),
            "{call} declares a concrete return type"
        );
    }
}

#[test]
fn a_variant_returning_native_method_is_inferred_from_a_variant() {
    // `Object.get_meta` and `Object.get_script` are declared Variant; `Object.get_class` is
    // declared String and `Time.get_datetime_dict_from_system` a Dictionary.
    assert_eq!(
        inference_lines("func f() -> void:\n\tvar m := get_meta(\"k\")\n\tprint(m)\n"),
        [4]
    );
    assert_eq!(
        inference_lines("func f() -> void:\n\tvar s := get_script()\n\tprint(s)\n"),
        [4]
    );
    assert_eq!(
        inference_lines("func f() -> void:\n\tvar c := get_class()\n\tprint(c)\n"),
        Vec::<u32>::new()
    );
    assert_eq!(
        inference_lines(
            "func f() -> void:\n\tvar t := Time.get_datetime_dict_from_system()\n\tprint(t)\n"
        ),
        Vec::<u32>::new()
    );
}

#[test]
fn every_declaration_kind_the_warning_covers_reports_once() {
    // Godot's message names the kind, so a constant and a parameter reach the same site.
    assert_eq!(
        inference_lines(
            "func f() -> void:\n\tvar a := abs(-1)\n\tvar b := abs(-2)\n\tprint(a, b)\n"
        ),
        [4, 5]
    );
}

#[test]
fn a_gdls_degrade_stays_silent() {
    // The other source of soft Variants is gdls admitting it does not know — an unresolvable
    // preload, a member off a dynamic base. Those must never draw an error-by-default warning:
    // there is nothing wrong with the line, and the port would be inventing a claim.
    assert_eq!(
        inference_lines(
            "func f() -> void:\n\tvar g = preload(\"res://gone.gd\")\n\tvar x := g\n\tprint(x)\n"
        ),
        Vec::<u32>::new()
    );
    assert_eq!(
        inference_lines("func f(v) -> void:\n\tvar y := v.whatever\n\tprint(y)\n"),
        Vec::<u32>::new()
    );
}

#[test]
fn an_explicit_annotation_never_reaches_the_warning() {
    // `var a: Variant = abs(-1)` is the user saying it on purpose — no inference, no warning.
    assert_eq!(
        inference_lines("func f() -> void:\n\tvar a: Variant = abs(-1)\n\tprint(a)\n"),
        Vec::<u32>::new()
    );
    assert_eq!(
        inference_lines("func f() -> void:\n\tvar a = abs(-1)\n\tprint(a)\n"),
        Vec::<u32>::new()
    );
}
