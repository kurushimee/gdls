//! #556 — `assert()`'s second argument must be a `String`.
//!
//! `resolve_assert` (`gdscript_analyzer.cpp:2400-2404`):
//!
//! ```cpp
//! if (!p_assert->message->get_datatype().has_no_type() &&
//!     (p_assert->message->get_datatype().kind != GDScriptParser::DataType::BUILTIN ||
//!      p_assert->message->get_datatype().builtin_type != Variant::STRING)) {
//!     push_error(R"(Expected string for assert error message.)", p_assert->message);
//! }
//! ```
//!
//! It is not constant-gated: a `Variant`-typed parameter fails because its kind is `VARIANT`, and a
//! `StringName` fails because only `String` passes. The `has_no_type` exemption is what keeps every
//! gdls degrade silent without a provenance gate of its own — anything gdls could not resolve
//! carries the no-type dummy and is skipped.
//!
//! Every expected row is verbatim `Godot_v4.7.2-stable --headless --check-only` output.

use std::path::Path;

use gd_analyze::{analyze, NoCrossFile, Severity, StrictSettings, WarnPolicy};
use gd_syntax::Dialect;
use gd_types::NativeDb;

fn native_db() -> NativeDb {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../gd_types/tests/fixtures/trimmed_api.json");
    NativeDb::load(path.to_str().expect("utf-8 path"))
        .unwrap_or_else(|e| panic!("load native DB fixture at {}: {e}", path.display()))
}

fn policy() -> WarnPolicy {
    WarnPolicy::build(
        &gd_project::WarningConfig::default(),
        &StrictSettings::default(),
        Dialect::DEFAULT,
    )
}

fn errors(src: &str) -> Vec<String> {
    let tree = gd_syntax::parse(src).tree;
    let result = analyze(&tree, None, "t.gd", &native_db(), &NoCrossFile, &policy());
    result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| d.message().to_owned())
        .collect()
}

const MSG: &str = "Expected string for assert error message.";

/// A literal of the wrong type.
#[test]
fn a_non_string_message_errors() {
    let src = "extends Node\n\nfunc f() -> void:\n\tassert(true, 2)\n";
    assert_eq!(errors(src), vec![MSG.to_owned()]);
}

/// Not constant-gated: a `Variant` parameter has a set type whose KIND is not builtin, so it fails
/// exactly as the literal does.
#[test]
fn a_variant_message_errors() {
    let src = "extends Node\n\nfunc f(v: Variant) -> void:\n\tassert(true, v)\n";
    assert_eq!(errors(src), vec![MSG.to_owned()]);
}

/// Only `String` passes — `StringName` is a different builtin.
#[test]
fn a_string_name_message_errors() {
    let src = "extends Node\n\nfunc f(sn: StringName) -> void:\n\tassert(true, sn)\n";
    assert_eq!(errors(src), vec![MSG.to_owned()]);
}

/// A `String` literal, a `String` variable, and a folded `String` expression all pass.
#[test]
fn a_string_message_is_silent() {
    let src = "extends Node\n\nfunc f(s: String) -> void:\n\tassert(true, \"ok\")\n\tassert(true, s)\n\tassert(true, \"a\" + \"b\")\n";
    assert_eq!(errors(src), Vec::<String>::new());
}

/// No message at all is the common shape and has nothing to check.
#[test]
fn a_bare_assert_is_silent() {
    let src = "extends Node\n\nfunc f() -> void:\n\tassert(true)\n";
    assert_eq!(errors(src), Vec::<String>::new());
}

/// A message gdls could not resolve carries the no-type dummy, so only the real problem inside it
/// is reported — never a second row claiming the message is the wrong type.
#[test]
fn an_unresolved_message_reports_only_its_own_miss() {
    let src = "extends Node\n\nfunc f() -> void:\n\tassert(true, nope())\n";
    assert_eq!(
        errors(src),
        vec![r#"Function "nope()" not found in base self."#.to_owned()]
    );
}
