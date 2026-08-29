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

use gd_syntax::Dialect;
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
    WarnPolicy::build(
        &WarningConfig::default(),
        &StrictSettings::default(),
        Dialect::DEFAULT,
    )
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
        msgs.iter()
            .any(|m| m
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

// --- CONSTRUCTOR with a parameterized `_init`: CORRECT ARITY SILENT --------------------------

#[test]
fn constructor_correct_arity_is_silent() {
    // `Inner.new(1, 2)` against `func _init(_a, _b)` — 2 required params, 2 args. Godot resolves
    // `_init`'s real arity (get_function_signature with p_is_constructor=true,
    // gdscript_analyzer.cpp:5829-5869) and validate_call_arg passes, so this must stay silent.
    let src = "\
extends RefCounted

class Inner:
\tfunc _init(_a, _b):
\t\tpass

func make() -> void:
\tvar _x = Inner.new(1, 2)
";
    let msgs = error_messages(src);
    assert!(
        !msgs.iter().any(|m| m.contains("arguments for")),
        "constructor with matching _init arity must not arity-error; got {msgs:?}"
    );
}

// --- CONSTRUCTOR over-call: TOO MANY fires -----------------------------------------------------

#[test]
fn constructor_too_many_arguments_fires() {
    // `Inner.new(1, 2, 3)` against `func _init(_a, _b)` — 2 params, 0 defaults, not vararg.
    // Godot resolves `_init`'s arity and validate_call_arg (gdscript_analyzer.cpp:5944-5950)
    // emits "Too many arguments...". The message uses `p_call->function_name`, which is `new`.
    let src = "\
extends RefCounted

class Inner:
\tfunc _init(_a, _b):
\t\tpass

func make() -> void:
\tvar _x = Inner.new(1, 2, 3)
";
    let msgs = error_messages(src);
    assert!(
        msgs.iter()
            .any(|m| m
                == "Too many arguments for \"new()\" call. Expected at most 2 but received 3."),
        "constructor over-call must emit Too many arguments; got {msgs:?}"
    );
}

// --- CONSTRUCTOR under-call: TOO FEW fires -----------------------------------------------------

#[test]
fn constructor_too_few_arguments_fires() {
    // `Inner.new(1)` against `func _init(_a, _b)` — 2 required params, 1 arg. Too few.
    let src = "\
extends RefCounted

class Inner:
\tfunc _init(_a, _b):
\t\tpass

func make() -> void:
\tvar _x = Inner.new(1)
";
    let msgs = error_messages(src);
    assert!(
        msgs.iter()
            .any(|m| m
                == "Too few arguments for \"new()\" call. Expected at least 2 but received 1."),
        "constructor under-call must emit Too few arguments; got {msgs:?}"
    );
}

// --- CONSTRUCTOR with defaulted `_init` params: required-only is SILENT -----------------------

#[test]
fn constructor_optional_defaults_required_only_is_silent() {
    // `func _init(_a, _b = 0)` — 2 params, 1 default ⇒ min 1, max 2. `Inner.new(1)` supplies the
    // one required arg and must stay silent (the default-arg lower bound).
    let src = "\
extends RefCounted

class Inner:
\tfunc _init(_a, _b = 0):
\t\tpass

func make() -> void:
\tvar _x = Inner.new(1)
";
    let msgs = error_messages(src);
    assert!(
        !msgs.iter().any(|m| m.contains("arguments for")),
        "constructor supplying only required _init args must be silent; got {msgs:?}"
    );
}

// --- NATIVE constructor over-call: TOO MANY (Expected at most 0) -----------------------------

#[test]
fn native_constructor_over_call_fires_expected_at_most_zero() {
    // Godot's constructor fallback for a base with no `_init` (analyzer.cpp:5897-5903) `return
    // true` with EMPTY par_types, so `validate_call_arg` (analyzer.cpp:5948-5950) fires
    // "Too many arguments... Expected at most 0" on `RefCounted.new(1, 2, 3)` — the message uses
    // `p_call->function_name`, which is `new`. gdls must match: a resolvable native base is
    // constructible, so the zero-arg arity bound applies.
    let src = "\
extends Node

func _ready() -> void:
\tvar _x = RefCounted.new(1, 2, 3)
";
    let msgs = error_messages(src);
    assert!(
        msgs.iter().any(|m| m
            == "Too many arguments for \"new()\" call. Expected at most 0 but received 3."),
        "native constructor over-call must emit Too many arguments (Expected at most 0); got {msgs:?}"
    );
}

// --- NATIVE constructor zero-arg: SILENT -----------------------------------------------------

#[test]
fn native_constructor_zero_args_is_silent() {
    // `RefCounted.new()` supplies 0 args against the zero-arg native fallback — correct arity,
    // no error (too-few can never fire since min == 0).
    let src = "\
extends Node

func _ready() -> void:
\tvar _x = RefCounted.new()
";
    let msgs = error_messages(src);
    assert!(
        !msgs.iter().any(|m| m.contains("arguments for")),
        "native constructor with no args must be silent; got {msgs:?}"
    );
}

// --- IN-FILE class with NO `_init`, over-call: TOO MANY (Expected at most 0) -----------------

#[test]
fn in_file_class_no_init_over_call_fires_expected_at_most_zero() {
    // An in-file class that declares no `_init` hits the same constructor fallback
    // (analyzer.cpp:5897-5903 → empty par_types), so `Inner.new(1)` over-supplies a zero-arg
    // constructor and must emit "Too many arguments... Expected at most 0".
    let src = "\
extends RefCounted

class Inner:
\tvar x := 1

func make() -> void:
\tvar _x = Inner.new(1)
";
    let msgs = error_messages(src);
    assert!(
        msgs.iter()
            .any(|m| m
                == "Too many arguments for \"new()\" call. Expected at most 0 but received 1."),
        "in-file class without _init must arity-error on over-call; got {msgs:?}"
    );
}

// --- UNRESOLVED cross-file / dynamic call with args: SILENT ----------------------------------

#[test]
fn unresolved_variant_call_with_arguments_is_silent() {
    // A call on a Variant receiver degrades to "Unknown stays dynamic" — `found = true` to suppress
    // the value-callable error, but `sig` is left at its zero-arg default. Arity-checking that path
    // would emit a phantom `Too many arguments... Expected at most 0` Godot never produces (it skips
    // validate_call_arg whenever get_function_signature returns false).
    let src = "\
extends Node

func run(thing) -> void:
\tthing.do_something(1, 2, 3)
";
    let msgs = error_messages(src);
    assert!(
        !msgs.iter().any(|m| m.contains("arguments for")),
        "unresolved dynamic call must not arity-error; got {msgs:?}"
    );
}

// --- SEEDED DUMP-OMITTED NATIVE METHOD: SILENT (no real param metadata) ----------------------

#[test]
fn seeded_dump_omitted_native_method_over_call_is_silent() {
    // `_notification`/`_get`/`_set` are `Object`-core virtuals `ClassDB` resolves but
    // `extension_api.json` omits; gdls seeds them as name-only stubs (zero params) purely so the
    // method-existence lookup binds (suppressing the #123 Callable warning). Those stubs carry no
    // real arity, so the count check must NOT run on them — Godot DOES arity-check these (ClassDB
    // has the params) but gdls lacks them, so silent under-emission is the faithful degrade
    // ("never lie"), never a phantom `Too many arguments... Expected at most 0`.
    let src = "\
extends Node

func _ready() -> void:
\t_notification(0)
\t_get(\"prop\")
\t_set(\"prop\", 1)
";
    let msgs = error_messages(src);
    assert!(
        !msgs.iter().any(|m| m.contains("arguments for")),
        "seeded dump-omitted native methods must not arity-error; got {msgs:?}"
    );
}
