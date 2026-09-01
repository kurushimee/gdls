//! #371 slice 1 — an annotation's argument COUNT, against the registered `MethodInfo`.
//!
//! `parse_annotation` checked an annotation's name and its target level and then accepted whatever
//! was inside the parentheses, so `@export_range()` and `@tool(1)` both parsed clean.
//!
//! The rule reads off the registration table two ways. The maximum is the parameter count and is
//! skipped entirely for a vararg registration (`gdscript_parser.cpp:4411`); the minimum is the
//! parameter count minus the default count and is enforced even for a vararg (:4416). The
//! string-literal requirement applies to the three annotations Godot resolves in the parser itself
//! (:4422). Everything else — per-argument types, and every `apply` callback — is the analyzer's,
//! which is where Godot runs it too (`gdscript_analyzer.cpp:1673`, and the comment at
//! `gdscript_parser.cpp:4442`).
//!
//! Every row below was run against `godot --headless --check-only` at 4.7.2 and matches it exactly,
//! silent rows included.

use gd_syntax::parse;

fn errors(src: &str) -> Vec<String> {
    parse(src)
        .diagnostics
        .iter()
        .map(|d| d.message.clone())
        .collect()
}

fn assert_one(src: &str, want: &str) {
    assert_eq!(errors(src), vec![want.to_owned()], "{src:?}");
}

fn assert_silent(src: &str) {
    assert_eq!(errors(src), Vec::<String>::new(), "{src:?}");
}

/// The minimum is parameters minus defaults, and a vararg registration still enforces it.
/// `@export_range` has four parameters and two defaults.
#[test]
fn too_few_arguments_report_the_minimum() {
    assert_one(
        "extends Node\n@export_range() var r: int = 0\n",
        r#"Annotation "@export_range" requires at least 2 arguments, but 0 were given."#,
    );
    assert_one(
        "extends Node\n@export_enum() var e: int = 0\n",
        r#"Annotation "@export_enum" requires at least 1 arguments, but 0 were given."#,
    );
    assert_one(
        "extends Node\n@export_flags() var f: int = 0\n",
        r#"Annotation "@export_flags" requires at least 1 arguments, but 0 were given."#,
    );
    assert_one(
        "extends Node\n@export_custom(1) var c = 0\n",
        r#"Annotation "@export_custom" requires at least 2 arguments, but 1 were given."#,
    );
    assert_one(
        "@tool\nextends Node\n@export_tool_button() var b: Callable\n",
        r#"Annotation "@export_tool_button" requires at least 1 arguments, but 0 were given."#,
    );
    assert_one(
        "@icon()\nextends Node\n",
        r#"Annotation "@icon" requires at least 1 arguments, but 0 were given."#,
    );
}

/// The maximum is the parameter count, and applies only to a non-vararg registration. Note the
/// messages are not pluralized — `requires at most 1 arguments` is Godot's literal output.
#[test]
fn too_many_arguments_report_the_maximum() {
    assert_one(
        "@icon(\"a\", \"b\")\nextends Node\n",
        r#"Annotation "@icon" requires at most 1 arguments, but 2 were given."#,
    );
    assert_one(
        "@tool(1)\nextends Node\n",
        r#"Annotation "@tool" requires at most 0 arguments, but 1 were given."#,
    );
    assert_one(
        "extends Node\n@onready(1) var x = 1\n",
        r#"Annotation "@onready" requires at most 0 arguments, but 1 were given."#,
    );
    assert_one(
        "extends Node\n@export_custom(1, \"a\", 2, 3) var c = 0\n",
        r#"Annotation "@export_custom" requires at most 3 arguments, but 4 were given."#,
    );
    // `@rpc` has four parameters and four defaults, so its range is 0 to 4.
    assert_one(
        "extends Node\n@rpc(\"any_peer\", \"call_local\", \"reliable\", 0, 1)\nfunc f() -> void:\n\tpass\n",
        r#"Annotation "@rpc" requires at most 4 arguments, but 5 were given."#,
    );
}

/// A vararg registration has no maximum at all, and a fully-defaulted one has no minimum either.
#[test]
fn a_vararg_registration_accepts_any_number_of_arguments() {
    assert_silent("extends Node\n@export_range(0, 10) var r: int = 0\n");
    assert_silent(
        "extends Node\n@export_range(0, 10, 1, \"or_greater\", \"hide_slider\") var r: int = 0\n",
    );
    assert_silent("extends Node\n@export_file() var f: String = \"\"\n");
    assert_silent("extends Node\n@rpc\nfunc f() -> void:\n\tpass\n");
}

/// A trailing comma adds no argument: both parsers break on the `)` before parsing another.
#[test]
fn a_trailing_comma_is_not_an_argument() {
    assert_silent("extends Node\n@export_range(0, 10,) var r: int = 0\n");
}

/// The three annotations the parser resolves itself need real string literals. A constant is not
/// enough — the check is on the NODE being a literal — and neither is a literal of the wrong Variant
/// type, which is why a `StringName` is rejected.
#[test]
fn the_parser_resolved_annotations_require_string_literals() {
    assert_one(
        "@icon(P2)\nextends Node\nconst P2 = \"res://icon.svg\"\n",
        r#"Argument 1 of annotation "@icon" must be a string literal."#,
    );
    assert_one(
        "@icon(5)\nextends Node\n",
        r#"Argument 1 of annotation "@icon" must be a string literal."#,
    );
    assert_one(
        "extends Node\n@warning_ignore_start(&\"unused_variable\")\nfunc f() -> void:\n\tpass\n",
        r#"Argument 1 of annotation "@warning_ignore_start" must be a string literal."#,
    );
    // The legal region must not regress.
    assert_silent(concat!(
        "extends Node\n",
        "@warning_ignore_start(\"unused_variable\")\n",
        "func f() -> void:\n",
        "\tvar q = 1\n",
        "@warning_ignore_restore(\"unused_variable\")\n",
    ));
}

/// An annotation that is already invalid for its name or its level is not counted as well: Godot
/// gates the whole check on `if (valid)` (`gdscript_parser.cpp:1907`), so one mistake draws one
/// error.
#[test]
fn an_annotation_invalid_for_its_level_is_not_also_counted() {
    assert_one(
        "extends Node\nconst P2 = \"res://icon.svg\"\n@icon(P2)\nfunc f() -> void:\n\tpass\n",
        r#"Annotation "@icon" must be at the top of the script, before "extends" and "class_name"."#,
    );
    assert_one(
        "extends Node\n@nonexistent(1, 2, 3)\nfunc f() -> void:\n\tpass\n",
        r#"Unrecognized annotation: "@nonexistent"."#,
    );
}

/// The table is a transcription, so the invariants that make the arity arithmetic well-formed are
/// asserted directly: a `varray` never supplies more defaults than there are parameters, and every
/// registered name is unique and carries its `@`.
#[test]
fn the_registration_table_is_well_formed() {
    let all = gd_syntax::parser::REGISTERED_ANNOTATIONS;
    assert_eq!(all.len(), 36, "Godot registers 36 annotations at both tags");
    for a in all {
        assert!(a.name.starts_with('@'), "{}", a.name);
        assert!(
            a.default_arg_count() <= a.params.len(),
            "{} has more defaults than parameters",
            a.name
        );
        assert_eq!(a.takes_arguments(), !a.params.is_empty(), "{}", a.name);
        assert_eq!(
            all.iter().filter(|b| b.name == a.name).count(),
            1,
            "{} is registered twice",
            a.name
        );
    }
}

/// The signature `hover` renders for an annotation, pinned against Godot's own
/// `_make_arguments_hint(info, -1, true)` (`gdscript_editor.cpp:750`): no return type, each
/// parameter as `name: Type`, the `varray` defaults on the trailing parameters as their construct
/// strings, and `...args: Array` for a vararg.
#[test]
fn registered_annotation_signatures_match_godots_argument_hint() {
    let sig = |name: &str| {
        gd_syntax::parser::registered_annotation(name)
            .expect("registered")
            .signature()
    };
    assert_eq!(sig("@tool"), "@tool()");
    assert_eq!(sig("@icon"), "@icon(icon_path: String)");
    assert_eq!(
        sig("@export_range"),
        "@export_range(min: float, max: float, step: float = 1.0, extra_hints: String = \"\", ...args: Array)"
    );
    // Vararg with no defaults, and vararg with one.
    assert_eq!(
        sig("@export_flags"),
        "@export_flags(names: String, ...args: Array)"
    );
    assert_eq!(
        sig("@export_file"),
        "@export_file(filter: String = \"\", ...args: Array)"
    );
    // Every parameter defaulted.
    assert_eq!(
        sig("@rpc"),
        "@rpc(mode: String = \"authority\", sync: String = \"call_remote\", transfer_mode: String = \"reliable\", transfer_channel: int = 0)"
    );
    // An int default rendered as its construct string (`PROPERTY_USAGE_DEFAULT` is 6).
    assert_eq!(
        sig("@export_custom"),
        "@export_custom(hint: int, hint_string: String, usage: int = 6)"
    );
}

/// Every registration renders without panicking, and the defaults never outnumber the parameters.
#[test]
fn every_registered_annotation_renders_a_well_formed_signature() {
    for a in gd_syntax::parser::REGISTERED_ANNOTATIONS {
        let sig = a.signature();
        assert!(sig.starts_with(a.name), "{sig} must open with {}", a.name);
        assert!(sig.ends_with(')'), "{sig} must close its parameter list");
        assert!(
            a.defaults.len() <= a.params.len(),
            "{} has more defaults than parameters",
            a.name
        );
    }
}
