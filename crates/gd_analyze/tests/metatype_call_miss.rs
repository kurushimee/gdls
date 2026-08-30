//! #417, #418, #426 — a method miss on a Class or Script base, and the shadows that must silence
//! it.
//!
//! Godot's miss branch (`gdscript_analyzer.cpp:3722-3774`) is ONE branch that decides per base kind
//! between `Function "%s()" not found in base %s.`, `Static function "%s()" not found in base
//! "%s".`, and the `UNSAFE_METHOD_ACCESS` warning. Its gates are kind-agnostic. gdls used to reach
//! that branch only for a self-call or a hard builtin, so three things went wrong at once: an
//! inner-class base never drew the warning (#418), a cross-file `class_name` metatype never drew
//! the static-miss error (#417), and the warning that DID fire for a Script base fired ahead of the
//! callee probe, so calling a native property through a metatype drew a phantom miss (#426).
//!
//! Every row is pinned against `Godot_v4.7.2-stable --headless --check-only` inside an imported
//! project, with `unsafe_method_access=2` so the warnings print.

use std::collections::HashMap;
use std::path::Path;

use gd_analyze::{analyze, CrossFileQuery, Severity, StrictSettings, WarnPolicy};
use gd_project::{FileId, Interface};
use gd_syntax::{parse, Dialect};
use gd_types::NativeDb;

fn native_db() -> NativeDb {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../gd_types/tests/fixtures/trimmed_api.json");
    NativeDb::load(path.to_str().expect("utf-8 path"))
        .unwrap_or_else(|e| panic!("load native DB fixture at {}: {e}", path.display()))
}

/// `UNSAFE_METHOD_ACCESS` defaults to Ignore, so every row turns it on explicitly.
fn policy() -> WarnPolicy {
    WarnPolicy::build(
        &gd_project::WarningConfig::default(),
        &StrictSettings {
            enable_warnings: vec!["UNSAFE_METHOD_ACCESS".to_owned()],
            ..Default::default()
        },
        Dialect::DEFAULT,
    )
}

struct Project {
    ifaces: HashMap<FileId, Interface>,
    by_class_name: HashMap<String, FileId>,
    by_path: HashMap<String, FileId>,
    paths: HashMap<FileId, String>,
}

impl Project {
    fn new(files: &[(&str, &str)]) -> Self {
        let mut p = Project {
            ifaces: HashMap::new(),
            by_class_name: HashMap::new(),
            by_path: HashMap::new(),
            paths: HashMap::new(),
        };
        for (i, (path, src)) in files.iter().enumerate() {
            let fid = FileId::new(i as u32 + 1);
            let iface = gd_project::extract_interface(&parse(src).tree);
            if let Some(name) = &iface.class_name {
                p.by_class_name.insert(name.clone(), fid);
            }
            p.by_path.insert((*path).to_owned(), fid);
            p.paths.insert(fid, (*path).to_owned());
            p.ifaces.insert(fid, iface);
        }
        p
    }
}

impl CrossFileQuery for Project {
    fn global_class_file(&self, name: &str) -> Option<FileId> {
        self.by_class_name.get(name).copied()
    }
    fn interface(&self, file: FileId) -> Option<&Interface> {
        self.ifaces.get(&file)
    }
    fn resolve_res_path(&self, path: &str) -> Option<FileId> {
        self.by_path.get(path).copied()
    }
    fn file_path(&self, file: FileId) -> Option<&str> {
        self.paths.get(&file).map(String::as_str)
    }
}

/// `class_name Lib extends Node`, carrying one of every member kind the shadow rows need.
const LIB_GD: &str = "\
class_name Lib
extends Node

signal some_signal(a: int)

const SOME_CONST := 5

enum SomeEnum { A }

static var static_count := 0

var hp := 1

static func compare(a: int, b: int) -> int:
\treturn a - b
";

/// A `class_name` whose file does not parse cleanly, so its interface cannot testify to absence.
const BROKEN_GD: &str = "\
class_name BrokenLib
extends Node

func real() -> void:
\tpass

func (( -> :
";

fn diagnose(consumer: &str) -> (Vec<String>, Vec<String>) {
    let project = Project::new(&[
        ("res://lib.gd", LIB_GD),
        ("res://broken.gd", BROKEN_GD),
        ("res://main.gd", consumer),
    ]);
    let tree = parse(consumer).tree;
    let result = analyze(
        &tree,
        Some(FileId::new(3)),
        "res://main.gd",
        &native_db(),
        &project,
        &policy(),
    );
    let errors = result
        .diagnostics
        .iter()
        .filter(|d| d.severity() == Severity::Error)
        .map(|d| d.message().to_owned())
        .collect();
    let warnings = result
        .diagnostics
        .iter()
        .filter(|d| d.code() == "UNSAFE_METHOD_ACCESS")
        .map(|d| d.message().to_owned())
        .collect();
    (errors, warnings)
}

fn body(stmt: &str) -> String {
    format!("extends Node\n\nfunc f() -> void:\n\t{stmt}\n")
}

// ===================================================================================================
// #417 — the static-miss error, on every metatype kind whose surface gdls can walk.
// ===================================================================================================

/// The headline row. Oracle, both halves:
/// `Static function "nope_static()" not found in base "Lib".` plus
/// `The method "nope_static()" is not present on the inferred type "Lib" …`
#[test]
fn a_cross_file_class_name_static_miss_reports_both_halves() {
    let (errors, warnings) = diagnose(&body("Lib.nope_static()"));
    assert_eq!(
        errors,
        vec![r#"Static function "nope_static()" not found in base "Lib"."#.to_owned()]
    );
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(
        warnings[0]
            .contains(r#"The method "nope_static()" is not present on the inferred type "Lib""#),
        "{warnings:?}"
    );
}

/// The same two halves for a class declared in the calling file, and for an inner class. Before
/// this, both drew the error and neither drew the warning.
#[test]
fn an_in_file_class_static_miss_reports_both_halves() {
    for (decl, name) in [
        ("class_name Main\nextends Node\n", "Main"),
        ("extends Node\n\nclass Inner:\n\tvar iv := 1\n", "Inner"),
    ] {
        let src = format!("{decl}\nfunc f() -> void:\n\t{name}.nope_static()\n");
        let (errors, warnings) = diagnose(&src);
        assert_eq!(
            errors,
            vec![format!(
                r#"Static function "nope_static()" not found in base "{name}"."#
            )],
            "{name}"
        );
        assert_eq!(warnings.len(), 1, "{name}: {warnings:?}");
    }
}

/// A real static function resolves, and so does an inherited native one.
#[test]
fn a_real_static_and_a_real_native_method_stay_silent() {
    let (errors, warnings) = diagnose(&body("Lib.compare(1, 2)"));
    assert_eq!(errors, Vec::<String>::new());
    assert_eq!(warnings, Vec::<String>::new());
}

// ===================================================================================================
// #418 — the warning, on a Class or Script INSTANCE base.
// ===================================================================================================

/// The #418 repro. Oracle: the warning fires on both the inferred and the annotated base, and the
/// property miss beside it keeps firing as it always did.
#[test]
fn an_inner_class_instance_method_miss_warns() {
    let src = "\
extends Node

class Inner:
\tvar iv := 1
\tfunc im() -> void:
\t\tpass

func f() -> void:
\tvar inferred := Inner.new()
\tvar annotated: Inner = Inner.new()
\tinferred.nope_m()
\tannotated.nope_m()
\tinferred.im()
";
    let (errors, warnings) = diagnose(src);
    assert_eq!(
        errors,
        Vec::<String>::new(),
        "no error for an instance base"
    );
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    for w in &warnings {
        assert!(
            w.contains(r#"The method "nope_m()" is not present on the inferred type "Inner""#),
            "{w}"
        );
    }
}

/// The cross-file instance base, which already warned, keeps warning and gains no error.
#[test]
fn a_cross_file_instance_method_miss_still_warns_and_never_errors() {
    let src = "\
extends Node

var l: Lib = null

func f() -> void:
\tl.nope_m()
";
    let (errors, warnings) = diagnose(src);
    assert_eq!(errors, Vec::<String>::new());
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains(r#""nope_m()" is not present on the inferred type "Lib""#));
}

// ===================================================================================================
// #426 — the shadows a metatype resolves, which must silence both halves.
// ===================================================================================================

/// A SCRIPT-declared const, enum, inner class, or static var resolves through the metatype
/// (analyzer.cpp:4210-4232), so Godot answers with the value-callable pair and never the miss.
#[test]
fn a_member_the_metatype_resolves_draws_the_value_pair_not_a_miss() {
    for name in ["SOME_CONST", "SomeEnum", "static_count"] {
        let (errors, warnings) = diagnose(&body(&format!("Lib.{name}()")));
        assert!(
            errors
                .iter()
                .any(|e| e == &format!(r#"Member "{name}" is not a function."#)),
            "{name}: {errors:?}"
        );
        assert!(
            !errors.iter().any(|e| e.contains("Static function")),
            "{name} resolves as a value; got {errors:?}"
        );
        assert_eq!(warnings, Vec::<String>::new(), "{name}: {warnings:?}");
    }
}

/// A signal and an instance variable do NOT resolve through a metatype (analyzer.cpp:4232/4243),
/// so Godot draws all three: member-not-function, the static miss, and the warning.
#[test]
fn a_signal_or_instance_var_through_a_metatype_draws_all_three() {
    for name in ["some_signal", "hp"] {
        let (errors, warnings) = diagnose(&body(&format!("Lib.{name}()")));
        assert!(
            errors.contains(&format!(r#"Member "{name}" is not a function."#)),
            "{name}: {errors:?}"
        );
        assert!(
            errors.contains(&format!(
                r#"Static function "{name}()" not found in base "Lib"."#
            )),
            "{name}: {errors:?}"
        );
        assert_eq!(warnings.len(), 1, "{name}: {warnings:?}");
    }
}

/// A NATIVE property, signal, or constant resolves through the metatype too — the native tail of
/// `reduce_identifier_from_base` has no meta gate at all (analyzer.cpp:4333-4386). These were the
/// #426 false positives.
#[test]
fn a_native_member_through_a_metatype_never_reports_a_miss() {
    for name in ["process_mode", "renamed", "NOTIFICATION_READY"] {
        let (errors, warnings) = diagnose(&body(&format!("Lib.{name}()")));
        assert!(
            !errors.iter().any(|e| e.contains("Static function")),
            "{name}: {errors:?}"
        );
        assert_eq!(warnings, Vec::<String>::new(), "{name}: {warnings:?}");
    }
}

/// `get_function_signature` also tries the `GDScript` class surface for any SCRIPT or CLASS
/// metatype (analyzer.cpp:6013), so these resolve in Godot and draw a different error gdls has not
/// ported. Suppressed rather than reported as missing — a deliberate under-report.
#[test]
fn a_gdscript_script_surface_name_through_a_metatype_is_suppressed() {
    for name in ["reload", "duplicate", "get_instance_base_type"] {
        let (errors, warnings) = diagnose(&body(&format!("Lib.{name}()")));
        assert!(
            !errors.iter().any(|e| e.contains("Static function")),
            "{name}: {errors:?}"
        );
        assert_eq!(warnings, Vec::<String>::new(), "{name}: {warnings:?}");
    }
}

// ===================================================================================================
// The firewall. Every row here is a negative claim gdls must refuse to make.
// ===================================================================================================

/// A base whose file did not parse cleanly cannot testify to absence: error recovery may have
/// dropped the very declaration being looked for.
#[test]
fn a_base_whose_file_does_not_parse_cleanly_stays_silent() {
    let (errors, warnings) = diagnose(&body("BrokenLib.nope_static()"));
    assert_eq!(errors, Vec::<String>::new(), "{errors:?}");
    assert_eq!(warnings, Vec::<String>::new(), "{warnings:?}");
}

/// An unresolvable base carries its own inheritance error; a phantom static miss on top of it is
/// exactly the false positive the Class arm used to produce, since it had no ancestry check.
#[test]
fn an_unresolvable_base_draws_its_own_error_and_no_miss() {
    let src = "\
extends Node

class Orphan extends NoSuchThing:
\tvar v := 1

func f() -> void:
\tOrphan.nope_static()
";
    let (errors, warnings) = diagnose(src);
    assert!(
        !errors.iter().any(|e| e.contains("Static function")),
        "{errors:?}"
    );
    assert_eq!(warnings, Vec::<String>::new(), "{warnings:?}");
}

/// A native metatype is Godot's `"GDScriptNativeClass"` rendering, which gdls does not produce.
/// Left silent on purpose rather than emitting a base name upstream never prints.
#[test]
fn a_native_metatype_miss_stays_silent() {
    let (errors, warnings) = diagnose(&body("Node2D.nope_static()"));
    assert!(
        !errors.iter().any(|e| e.contains("Static function")),
        "{errors:?}"
    );
    assert_eq!(warnings, Vec::<String>::new(), "{warnings:?}");
}

/// A self-call and a super-call are the not-found error's business, never the warning's
/// (analyzer.cpp:3751's `!is_self`).
#[test]
fn a_self_or_super_call_never_draws_the_warning() {
    let src = "\
extends Lib

func f() -> void:
\tnope_m()
\tself.nope_m()
\tsuper.nope_m()
";
    let (_errors, warnings) = diagnose(src);
    assert_eq!(warnings, Vec::<String>::new(), "{warnings:?}");
}
