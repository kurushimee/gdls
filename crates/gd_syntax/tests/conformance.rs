//! Parse-phase conformance harness.
//!
//! Diffs `gd_syntax::parse` output against Godot's vendored golden-file corpus
//! (`tests/conformance/corpus/parser/`, see its `PROVENANCE.md`). The oracle and the exact
//! comparison semantics are ported from Godot's `modules/gdscript/tests/gdscript_test_runner.cpp`:
//!
//! * skip `*.notest.gd`; keep `*.textonly.gd` / `*.bin.gd` / `*.norun.gd` (M1 is text-tokenizer mode);
//! * pair the `.out` by swapping the final extension;
//! * **classify by the `.out` first line, never by directory** — three `GDTEST_ANALYZER_ERROR` files
//!   live inside `parser/errors/`;
//! * a `GDTEST_PARSER_ERROR` `.out` carries only the first error message and no line/column, so M1
//!   compares the message **string**;
//! * `GDTEST_OK` ⇒ must parse with zero parser errors (runtime output + `~~` warnings are M3).
//!
//! Ratchet (hybrid): a per-file `known_failures.txt` regression net (primary) plus an aggregate
//! `fidelity_floor.txt` (secondary). Setting `GDLS_BLESS_CONFORMANCE=1` prints the regenerated state
//! to stdout for a human to commit — it never writes files.
//!
//! ## Suites
//!
//! [`SUITES`] pairs a corpus tree with the dialect its goldens were generated at. The newest
//! supported release carries the full vendored tree; an older one carries only the files whose
//! *parse-phase* result actually differs at that tag. Every reported path is prefixed with its
//! suite tag, so a `known_failures.txt` line names both the file and the dialect it failed under.
//!
//! 4.6 currently has **no** subset: the two tags' parser corpora differ only in test-harness helper
//! calls and in files that parse cleanly at both, so no file's parse-phase result diverges. The
//! guarded parser and lexer behaviors are pinned directly instead, in `tests/dialect_delta.rs`.
//! Adding a subset later is a directory plus one [`SUITES`] row — see
//! `scripts/conformance/demote_corpus.py`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use gd_syntax::{Dialect, ParseOptions};

/// What the `.out` pins about the parse phase. `None` from [`classify`] means "skip at M1".
enum Expect {
    /// `GDTEST_OK` — the file must parse with zero parser errors.
    NoParserError,
    /// `GDTEST_PARSER_ERROR` — the first parser error message must equal this string.
    ParserError(String),
}

/// One corpus tree and the dialect its `.out` goldens were generated at.
struct Suite {
    /// Path under `tests/conformance/`.
    dir: &'static str,
    /// Prefix on every reported path, so a known-failures entry names its suite.
    tag: &'static str,
    dialect: Dialect,
}

const SUITES: &[Suite] = &[Suite {
    dir: "corpus/parser",
    tag: "4.7",
    dialect: Dialect::Godot4_7,
}];

fn conformance_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/conformance")
}

fn collect_gd_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_gd_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("gd") {
            out.push(path);
        }
    }
}

/// Classify a `.out` by its first line. `None` ⇒ analyzer/compiler/runtime/load phase (skip at M1).
fn classify(out_content: &str) -> Option<Expect> {
    let mut lines = out_content.lines();
    match lines.next().unwrap_or("").trim() {
        "GDTEST_OK" => Some(Expect::NoParserError),
        "GDTEST_PARSER_ERROR" => {
            // Everything after the status line is the (single) first-error message; mirror the
            // runner's `strip_edges()` with a trim.
            let message = lines.collect::<Vec<_>>().join("\n").trim().to_string();
            Some(Expect::ParserError(message))
        }
        _ => None,
    }
}

fn rel_path(corpus: &Path, gd: &Path) -> String {
    gd.strip_prefix(corpus)
        .unwrap_or(gd)
        .to_string_lossy()
        .replace('\\', "/")
}

fn first_message(result: &gd_syntax::ParseResult) -> Option<&str> {
    result.diagnostics.first().map(|d| d.message.trim())
}

fn read_lines_set(path: &Path) -> BTreeSet<String> {
    fs::read_to_string(path)
        .map(|c| {
            c.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn read_floor(path: &Path) -> f64 {
    fs::read_to_string(path)
        .ok()
        .and_then(|c| {
            c.lines()
                .map(str::trim)
                .find(|l| !l.is_empty() && !l.starts_with('#'))
                .and_then(|l| l.parse::<f64>().ok())
        })
        .unwrap_or(0.0)
}

fn bullet_list(items: &BTreeSet<&String>) -> String {
    items
        .iter()
        .map(|s| format!("  {s}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn parse_phase_fidelity() {
    let conformance = conformance_dir();

    let mut eligible = 0usize;
    let mut matched = 0usize;
    let mut skipped = 0usize;
    let mut total_files = 0usize;
    let mut failures: BTreeSet<String> = BTreeSet::new();
    let mut samples: Vec<String> = Vec::new();

    for suite in SUITES {
        let corpus = conformance.join(suite.dir);
        assert!(
            corpus.is_dir(),
            "corpus missing at {} — see PROVENANCE.md to vendor it",
            corpus.display()
        );

        let mut gd_files = Vec::new();
        collect_gd_files(&corpus, &mut gd_files);
        gd_files.sort();
        assert!(
            !gd_files.is_empty(),
            "no .gd files found under {}",
            corpus.display()
        );
        total_files += gd_files.len();

        for gd in &gd_files {
            let name = gd.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            if name.ends_with(".notest.gd") {
                skipped += 1;
                continue;
            }

            let rel = format!("{}/{}", suite.tag, rel_path(&corpus, gd));
            let source = fs::read_to_string(gd).expect("read .gd source");
            let out_path = gd.with_extension("out");
            let out_content = fs::read_to_string(&out_path).unwrap_or_else(|_| {
                panic!("missing .out for {rel} (expected {})", out_path.display())
            });

            let Some(expect) = classify(&out_content) else {
                skipped += 1;
                continue;
            };

            eligible += 1;
            let result = gd_syntax::parse_with_options(
                &source,
                &ParseOptions {
                    dialect: suite.dialect,
                    script_path: "",
                },
            );
            let passed = match &expect {
                // M1 emits only syntax-error diagnostics, so "no parser error" == empty set. Once
                // the M3 analyzer puts *warnings* into the same `diagnostics` vec, this must filter
                // by severity (errors only) — otherwise every `GDTEST_OK` file carrying a `~~`
                // warning would fail.
                Expect::NoParserError => result.diagnostics.is_empty(),
                Expect::ParserError(msg) => first_message(&result) == Some(msg.as_str()),
            };

            if passed {
                matched += 1;
            } else {
                if samples.len() < 40 {
                    let got = match first_message(&result) {
                        Some(m) => format!("error {m:?}"),
                        None => "no parser error".to_string(),
                    };
                    let want = match &expect {
                        Expect::NoParserError => "no parser error".to_string(),
                        Expect::ParserError(m) => format!("error {m:?}"),
                    };
                    samples.push(format!("  {rel}\n      want: {want}\n      got:  {got}"));
                }
                failures.insert(rel);
            }
        }
    }

    let fidelity = if eligible == 0 {
        1.0
    } else {
        matched as f64 / eligible as f64
    };
    let summary = format!(
        "parse-phase fidelity: {matched}/{eligible} = {fidelity:.4}  \
         ({skipped} skipped, {total_files} total .gd)"
    );
    println!("{summary}");

    // Bless mode: emit the regenerated ratchet state for a human to commit. Never writes files.
    if std::env::var_os("GDLS_BLESS_CONFORMANCE").is_some() {
        let floor = (fidelity * 100.0).floor() / 100.0;
        println!(
            "\n----- BEGIN known_failures.txt ({} entries) -----",
            failures.len()
        );
        for f in &failures {
            println!("{f}");
        }
        println!("----- END known_failures.txt -----");
        println!(
            "----- BEGIN fidelity_floor.txt -----\n{floor:.2}\n----- END fidelity_floor.txt -----"
        );
        return;
    }

    let known = read_lines_set(&conformance.join("known_failures.txt"));
    let floor = read_floor(&conformance.join("fidelity_floor.txt"));

    let new_regressions: BTreeSet<&String> = failures.difference(&known).collect();
    let newly_passing: BTreeSet<&String> = known.difference(&failures).collect();

    let mut problems: Vec<String> = Vec::new();
    if !new_regressions.is_empty() {
        problems.push(format!(
            "{} NEW parse regression(s) — failing but not in known_failures.txt:\n{}",
            new_regressions.len(),
            bullet_list(&new_regressions)
        ));
    }
    if !newly_passing.is_empty() {
        problems.push(format!(
            "{} file(s) now PASS but are still listed in known_failures.txt (delete these lines):\n{}",
            newly_passing.len(),
            bullet_list(&newly_passing)
        ));
    }
    if fidelity + 1e-9 < floor {
        problems.push(format!(
            "fidelity {fidelity:.4} fell below floor {floor:.4}"
        ));
    }

    assert!(
        problems.is_empty(),
        "{summary}\n\n{}\n\nTo re-baseline: \
         GDLS_BLESS_CONFORMANCE=1 cargo test -p gd_syntax --test conformance -- --nocapture\n\n\
         sample mismatches:\n{}",
        problems.join("\n\n"),
        samples.join("\n"),
    );
}
