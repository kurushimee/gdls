//! Emission net for native-method call arity checking (#173).
//!
//! Godot's analyzer routes native method calls through `validate_call_arg`
//! (`gdscript_analyzer.cpp:3653 → :5944-5950`), which emits "Too few arguments..." /
//! "Too many arguments..." exactly as it does for in-file functions: the par-type count and the
//! method's `default_arguments.size()` give the required/total bounds, and the too-many check is
//! suppressed for vararg methods. gdls historically gated the count check behind
//! `in_file_function_id.is_some()`, so it stayed silent on every native over/under-call.
//!
//! These tests pin the corrected behaviour against the committed `trimmed_api.json` fixture (the
//! same dump the conformance harness loads). The conformance ratchet is emission-blind for added
//! errors in a clean corpus, so this direct net is the real coverage.

use std::path::Path;

use gd_analyze::{analyze, NoCrossFile, Severity, StrictSettings, WarnPolicy};
use gd_project::{FileId, WarningConfig};
use gd_syntax::parse;
use gd_types::NativeDb;

fn native_db() -> NativeDb {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../gd_types/tests/fixtures/trimmed_api.json");
    NativeDb::load(path.to_str().expect("utf-8 path"))
        .unwrap_or_else(|e| panic!("load native DB fixture at {}: {e}", path.display()))
}

fn policy() -> WarnPolicy {
    WarnPolicy::build(&WarningConfig::default(), &StrictSettings::default())
}

/// Analyze `src` and return every bare (non-warning) error as `(message, span_start_byte)`.
fn errors(src: &str) -> Vec<(String, usize)> {
    let tree = parse(src).tree;
    let db = native_db();
    let result = analyze(
        &tree,
        Some(FileId::new(1)),
        "arity.gd",
        &db,
        &NoCrossFile,
        &policy(),
    );
    result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error && d.warning_code().is_none())
        .map(|d| (d.message().to_owned(), d.span().start))
        .collect()
}

fn error_messages(src: &str) -> Vec<String> {
    errors(src).into_iter().map(|(m, _)| m).collect()
}

/// Byte offset of the n-th (1-based) occurrence of `needle`.
fn byte_of_nth(src: &str, needle: &str, n: usize) -> usize {
    let mut from = 0;
    let mut last = None;
    for _ in 0..n {
        let idx = src[from..]
            .find(needle)
            .map(|i| from + i)
            .unwrap_or_else(|| panic!("occurrence {n} of {needle:?} not found"));
        last = Some(idx);
        from = idx + needle.len();
    }
    last.expect("at least one occurrence")
}

// --- TOO FEW ---------------------------------------------------------------------------------

#[test]
fn native_too_few_arguments_fires() {
    // `Object.set(property, value)` — 2 required params, 0 defaults. Calling with 1 arg is too few.
    let src = "\
extends Node

func _ready() -> void:
\tset(\"x\")
";
    let msgs = error_messages(src);
    assert!(
        msgs.iter().any(|m| m
            == "Too few arguments for \"set()\" call. Expected at least 2 but received 1."),
        "native under-call must emit Too few arguments; got {msgs:?}"
    );
}

// --- TOO MANY (+ span = first excess arg) ----------------------------------------------------

#[test]
fn native_too_many_arguments_fires_with_first_excess_span() {
    // `Object.get(property)` — 1 param, 0 defaults, NOT vararg. Calling with 3 args is too many;
    // Godot anchors the error at the FIRST EXCESS arg (`arguments[par_types.size()]`), i.e. the
    // 2nd argument here.
    let src = "\
extends Node

func _ready() -> void:
\tget(\"a\", \"bb\", \"ccc\")
";
    let errs = errors(src);
    let hit = errs
        .iter()
        .find(|(m, _)| {
            m == "Too many arguments for \"get()\" call. Expected at most 1 but received 3."
        })
        .unwrap_or_else(|| panic!("native over-call must emit Too many arguments; got {errs:?}"));
    // Span must start at the first excess argument — the second arg literal `"bb"`.
    let want = byte_of_nth(src, "\"bb\"", 1);
    assert_eq!(
        hit.1, want,
        "too-many span must anchor at the first excess arg (\"bb\"), not the whole call"
    );
}

// --- CORRECT ARITY: SILENT -------------------------------------------------------------------

#[test]
fn native_correct_arity_is_silent() {
    let src = "\
extends Node

func _ready() -> void:
\tget(\"a\")
\tset(\"x\", 1)
";
    let msgs = error_messages(src);
    assert!(
        !msgs.iter().any(|m| m.contains("arguments for")),
        "correct native arity must not emit any arity error; got {msgs:?}"
    );
}

// --- VARARG NATIVE: SILENT when over-supplied ------------------------------------------------

#[test]
fn native_vararg_over_supply_is_silent() {
    // `Object.call(method, ...)` is vararg — too-many must NOT fire no matter how many args.
    let src = "\
extends Node

func _ready() -> void:
\tcall(\"some_method\", 1, 2, 3, 4)
";
    let msgs = error_messages(src);
    assert!(
        !msgs.iter().any(|m| m.contains("Too many arguments")),
        "vararg native must never emit Too many arguments; got {msgs:?}"
    );
}

// --- OPTIONAL/DEFAULT NATIVE: SILENT when only required args supplied -------------------------

#[test]
fn native_optional_defaults_required_only_is_silent() {
    // `Node.find_child(pattern, recursive=true, owned=true)` — 3 params, 2 defaults ⇒ min 1.
    // Calling with just the required `pattern` must stay silent (the #147-family FP guard).
    let src = "\
extends Node

func _ready() -> void:
\tfind_child(\"pat\")
";
    let msgs = error_messages(src);
    assert!(
        !msgs.iter().any(|m| m.contains("arguments for")),
        "optional-default native called with required args must be silent; got {msgs:?}"
    );
}
