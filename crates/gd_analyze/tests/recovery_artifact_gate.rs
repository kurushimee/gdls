//! #575 — what a file that failed to parse is still allowed to say about itself.
//!
//! Godot's language server skips the analyzer outright when the parse failed
//! (`language_server/gdscript_extend_parser.cpp:971-976`), so one typo makes it go semantically
//! blind for the whole file. gdls keeps analyzing, because the parts that parsed still deserve
//! their type errors — but error recovery invents statements, and judging those inventions reports
//! gdls's recovery rather than the user's code.
//!
//! One rule, three shapes. A partial parse can testify to what EXISTS and never to what is ABSENT,
//! and it is not judged on style:
//!
//! * a contradiction between two things both present in the tree is kept — recovery can lose a
//!   declaration, but it cannot invent the pair;
//! * a claim that something is absent is dropped, because recovery may have dropped the very
//!   declaration being asked about;
//! * warnings are dropped, because they are style judgments on statements the user did not write.
//!
//! The gate reads [`gd_syntax::ParseTree::recovery_lost_source`], not `had_parse_errors`, and the
//! difference matters: a parse error that abandoned nothing leaves a complete, correct tree, so
//! everything the user actually wrote still gets judged.
//!
//! The one known hole is pinned below: recovery that SHORTENS a declaration instead of dropping it
//! can still fabricate a contradiction, and catching that would need parser-side drop tracking the
//! faithful port does not have.

use std::collections::HashMap;
use std::path::Path;

use gd_analyze::{analyze, CrossFileQuery, Severity, StrictProfile, StrictSettings, WarnPolicy};
use gd_project::{FileId, Interface};
use gd_syntax::{parse, Dialect};
use gd_types::NativeDb;

fn native_db() -> NativeDb {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../gd_types/tests/fixtures/trimmed_api.json");
    NativeDb::load(path.to_str().expect("utf-8 path"))
        .unwrap_or_else(|e| panic!("load native DB fixture at {}: {e}", path.display()))
}

fn policy_with(strict: StrictSettings) -> WarnPolicy {
    WarnPolicy::build(
        &gd_project::WarningConfig::default(),
        &strict,
        Dialect::DEFAULT,
    )
}

/// A mock workspace over the given files, built by the real interface extractor so `parse_clean`
/// crosses the seam exactly as it does in production.
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
    /// Answers for any unresolved path, so the filesystem claim has something to report.
    fn preload_missing_path(&self, _from: Option<FileId>, raw: &str) -> Option<String> {
        (!self.by_path.contains_key(raw)).then(|| raw.to_owned())
    }
}

/// `(errors, warnings)` for `src` analyzed as `res://main.gd`, with `others` in the workspace.
fn split(src: &str, others: &[(&str, &str)], strict: StrictSettings) -> (Vec<String>, Vec<String>) {
    let mut files: Vec<(&str, &str)> = vec![("res://main.gd", src)];
    files.extend_from_slice(others);
    let project = Project::new(&files);
    let tree = parse(src).tree;
    let result = analyze(
        &tree,
        Some(FileId::new(1)),
        "res://main.gd",
        &native_db(),
        &project,
        &policy_with(strict),
    );
    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    for d in result.diagnostics {
        if d.severity() == Severity::Error {
            errors.push(d.message().to_owned());
        } else {
            warnings.push(d.message().to_owned());
        }
    }
    (errors, warnings)
}

fn errors(src: &str) -> Vec<String> {
    split(src, &[], StrictSettings::default()).0
}

fn warnings(src: &str) -> Vec<String> {
    split(src, &[], StrictSettings::default()).1
}

/// The whole point of analyzing a broken file: the parts that parsed still get their type errors.
/// Every row here is a contradiction between two things present in the tree.
#[test]
fn a_broken_parse_keeps_every_type_error_in_regions_it_never_touched() {
    let src = concat!(
        "extends Node\n",
        "\n",
        "func f() -> int:\n",
        "\treturn \"no\"\n",
        "\n",
        "func g() -> void:\n",
        "\tvar q: String = 1\n",
        "\tprint(q)\n",
        "\tvar n := Node.new()\n",
        "\tvar s := n as String\n",
        "\tprint(s)\n",
        "\tif n is 5:\n",
        "\t\tpass\n",
        "\tvar d: Dictionary[String, int] = {1: 2}\n",
        "\tprint(d)\n",
    );
    let (errs, warns) = split(src, &[], StrictSettings::default());
    for wanted in [
        r#"Cannot return a value of type "String" as "int"."#,
        r#"Cannot assign a value of type int to variable "q" with specified type String."#,
        r#"Invalid cast. Cannot convert from "Node" to "String"."#,
        r#"Cannot have a key of type "int" in a dictionary of type "Dictionary[String, int]"."#,
    ] {
        assert!(
            errs.iter().any(|e| e == wanted),
            "wanted {wanted:?}, got {errs:?}"
        );
    }
    assert!(
        warns.is_empty(),
        "a broken file is not judged on style; got {warns:?}"
    );
}

/// `is 5` is not a type, so recovery keeps the bare `n` and the standalone-expression warning is
/// about a statement the user never wrote.
#[test]
fn a_swallowed_is_operand_draws_no_standalone_expression_warning() {
    let src = "extends Node\n\nfunc f() -> void:\n\tvar n := Node.new()\n\tif n is 5:\n\t\tpass\n\tprint(n)\n";
    assert!(warnings(src).is_empty(), "got {:?}", warnings(src));
}

/// Recovery keeps `var a` with no initializer, so the read two lines down looks unassigned.
#[test]
fn a_dropped_initializer_draws_no_used_before_assigned_warning() {
    let src = "extends Node\n\nfunc f() -> void:\n\tvar a = = 1\n\tvar b = = 2\n\tprint(a, b)\n";
    assert!(warnings(src).is_empty(), "got {:?}", warnings(src));
}

/// The gate is the parse, not the presence of errors: fix the syntax and the warnings come back.
#[test]
fn a_clean_file_keeps_its_warning_set() {
    let src = "extends Node\n\nfunc f() -> void:\n\tvar unused := 1\n";
    let warns = warnings(src);
    assert!(
        warns
            .iter()
            .any(|w| w.contains(r#"The local variable "unused" is declared but never used"#)),
        "got {warns:?}"
    );
}

/// `starts_at_eof` means no meaningful tokens at all, which cannot co-occur with a parse error, so
/// the gate never reaches this one.
#[test]
fn an_empty_file_still_warns() {
    let warns = warnings("");
    assert!(
        warns.iter().any(|w| w.contains("Empty script file")),
        "got {warns:?}"
    );
}

/// Strict mode promotes warnings to errors. The gate keys on the warning's identity, not on its
/// rendered severity, so promotion cannot bring an artifact back as a hard error.
#[test]
fn strict_promotion_cannot_resurrect_a_warning_on_a_broken_file() {
    let src = "extends Node\n\nfunc f() -> void:\n\tvar a = = 1\n\tvar b = = 2\n\tprint(a, b)\n";
    let strict = StrictSettings {
        profile: StrictProfile::Strict,
        ..StrictSettings::default()
    };
    let (errs, warns) = split(src, &[], strict);
    assert!(warns.is_empty(), "got {warns:?}");
    assert!(
        !errs
            .iter()
            .any(|e| e.contains("used before being assigned")),
        "a promoted artifact is still an artifact; got {errs:?}"
    );
}

/// `var 5a = 1` leaves no `a` at all, so the read below is undeclared only in gdls's recovered
/// tree. Godot reports the parse error and nothing else.
#[test]
fn a_dropped_declaration_draws_no_not_declared_error() {
    let src = "extends Node\n\nfunc f() -> void:\n\tvar 5a = 1\n\tprint(a)\n";
    let errs = errors(src);
    assert!(
        !errs
            .iter()
            .any(|e| e.contains("not declared in the current scope")),
        "got {errs:?}"
    );
}

/// The same shape across the declaration kinds that can be dropped. One rule, not one special case.
#[test]
fn every_dropped_member_kind_draws_no_absence_error() {
    for src in [
        "extends Node\n\nconst 5C = 1\n\nfunc f() -> int:\n\treturn C\n",
        "extends Node\n\nsignal 5sig(n: int)\n\nfunc f() -> void:\n\tsig.emit(1)\n",
        "extends Node\n\nvar 5m := 1\n\nfunc f() -> int:\n\treturn m\n",
        "extends Node\n\nenum 5E { A }\n\nfunc f() -> void:\n\tprint(E.A)\n",
        "extends Node\n\nclass 5Thing:\n\tpass\n\nfunc f() -> void:\n\tprint(Thing.new())\n",
    ] {
        let errs = errors(src);
        for family in [
            "not declared in the current scope",
            "in the current scope.",
            "not found in base",
            "Cannot find member",
        ] {
            assert!(
                !errs.iter().any(|e| e.contains(family)),
                "for {src:?}: {family:?} survived in {errs:?}"
            );
        }
    }
}

/// The known hole, pinned so a future change to it trips a test rather than drifting.
///
/// Recovery here SHORTENS `k`'s parameter list instead of dropping the declaration, so the arity
/// check compares a real call against a real — but truncated — signature. That is a contradiction
/// between two present things, which is the class the rule keeps, and separating it would need the
/// parser to record what recovery discarded.
#[test]
fn a_truncated_parameter_list_still_misreports_arity() {
    let src = "extends Node\n\nfunc t() -> void:\n\tk(1, 2)\n\nfunc k(a: int, 5b: int) -> void:\n\tprint(a)\n";
    let errs = errors(src);
    assert!(
        errs.iter().any(|e| e.contains("Too many arguments")),
        "the residue is expected here; got {errs:?}"
    );
}

/// A claim about the filesystem is not a claim about the tree, so recovery cannot invalidate it and
/// the gate does not apply.
#[test]
fn a_missing_preload_still_errors_in_a_broken_file() {
    let src = "extends Node\n\nconst L = preload(\"res://gone.gd\")\n\nfunc f() -> void:\n\tvar a = = 1\n\tprint(a, L)\n";
    let errs = errors(src);
    assert!(
        errs.iter()
            .any(|e| e.contains(r#"Preload file "res://gone.gd" does not exist."#)),
        "got {errs:?}"
    );
}

/// The gate is per file. A clean file keeps everything, including the absence errors, so the rule
/// cannot be mistaken for a general softening.
#[test]
fn a_clean_file_keeps_its_absence_errors() {
    let errs = errors("extends Node\n\nfunc f() -> void:\n\tprint(zzz)\n");
    assert!(
        errs.iter()
            .any(|e| e == r#"Identifier "zzz" not declared in the current scope."#),
        "got {errs:?}"
    );
}

/// A broken NEIGHBOUR is `parse_clean`'s business, not this rule's: it must not silence this file.
#[test]
fn a_broken_neighbour_does_not_silence_this_files_warnings() {
    let src = "extends Node\n\nfunc f() -> void:\n\tvar unused := 1\n";
    let (_, warns) = split(
        src,
        &[("res://broken.gd", "extends Node\n\nvar 5x = 1\n")],
        StrictSettings::default(),
    );
    assert!(
        warns
            .iter()
            .any(|w| w.contains(r#"The local variable "unused" is declared but never used"#)),
        "got {warns:?}"
    );
}

/// `f(nope)()` draws `Cannot call on an expression. Use ".call()" if it's a Callable.` — and the
/// parser builds the whole call node anyway, abandoning no token. The tree is exactly the source,
/// so both gates stay open: `nope` really is undeclared, and the unused local really is unused.
#[test]
fn a_parse_error_that_abandoned_nothing_gates_neither() {
    let src =
        "extends Node\n\nfunc f(_a): pass\n\nfunc g() -> void:\n\tvar unused := 1\n\tf(nope)()\n";
    let (errs, warns) = split(src, &[], StrictSettings::default());
    assert!(
        errs.iter()
            .any(|e| e == r#"Identifier "nope" not declared in the current scope."#),
        "got {errs:?}"
    );
    assert!(
        warns
            .iter()
            .any(|w| w.contains(r#"The local variable "unused" is declared but never used"#)),
        "got {warns:?}"
    );
}

/// Recovery synthesizes a dummy value for a dictionary entry the source never supplied. That
/// invented node is the mirror of a discarded one, so it sets the same flag and nothing judges it.
#[test]
fn a_synthesized_dictionary_value_closes_the_gates() {
    let src = "extends Node\n\nfunc f() -> void:\n\tvar d: Dictionary[String, int] = {\"a\": }\n\tvar unused := 1\n\tprint(d)\n";
    let (_, warns) = split(src, &[], StrictSettings::default());
    assert!(
        warns.is_empty(),
        "an invented node is not the user's code; got {warns:?}"
    );
}
