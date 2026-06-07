//! Layer 1 of the two-layer fuzz gate (`docs/06-testing-fidelity.md`): an always-on, deterministic
//! panic-smoke test that runs under plain `cargo test` on stable. It feeds
//! [`gd_syntax::parse`] three families of input and asserts only that it never panics — the test
//! harness turns any panic or unwind (including a depth-guard-less stack overflow) into a failure,
//! and the workspace is built with `panic = "unwind"`. "Never crash" is a release invariant
//! (`CLAUDE.md`); the coverage-guided, libFuzzer-backed Layer 2 lives in `fuzz/` and runs on nightly
//! Linux only (libFuzzer needs nightly's `-Z` flags and is unsupported on Windows by cargo-fuzz).
//!
//! Determinism: a fixed-seed SplitMix64 PRNG drives every random choice, so a CI failure reproduces
//! locally from the same seed. `parse` takes `&str`, so all generated input is valid UTF-8 by
//! construction (we mutate by `char`, never by raw byte).

use std::fs;
use std::path::{Path, PathBuf};

/// SplitMix64 — a tiny, fast, fully deterministic PRNG (no external crate needed for a smoke test).
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }

    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len())]
    }
}

/// GDScript-flavoured fragments — keywords, operators, brackets, literals, and indentation — so the
/// generated soup exercises real parser paths (bracket nesting, statement keywords, annotations)
/// rather than arbitrary unicode.
const FRAGMENTS: &[&str] = &[
    "func ",
    "class ",
    "var ",
    "const ",
    "signal ",
    "enum ",
    "static ",
    "extends ",
    "class_name ",
    "if ",
    "elif ",
    "else",
    "for ",
    "while ",
    "match ",
    "return ",
    "break",
    "continue",
    "pass",
    "await ",
    "assert",
    "preload",
    "self",
    "super",
    "@export",
    "@onready",
    "@tool",
    "when ",
    "get",
    "set",
    "(",
    ")",
    "[",
    "]",
    "{",
    "}",
    ":",
    ",",
    ".",
    "..",
    "...",
    "=",
    ":=",
    "==",
    "+",
    "-",
    "*",
    "/",
    "**",
    "->",
    "$",
    "%",
    "&",
    "|",
    "^",
    "~",
    "!",
    "<",
    ">",
    "<<",
    ">>",
    "and ",
    "or ",
    "not ",
    "in ",
    "is ",
    "as ",
    "ident",
    "Name",
    "x",
    "1",
    "1.5",
    "0x1F",
    "1_000",
    "\"str\"",
    "&\"sn\"",
    "^\"np\"",
    "true",
    "false",
    "null",
    "PI",
    "\n",
    "\t",
    " ",
    "\n\t",
    "\n\t\t",
    "\n\t\t\t",
    ";",
];

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/conformance/corpus")
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

fn corpus_seeds() -> Vec<String> {
    let mut files = Vec::new();
    collect_gd_files(&corpus_dir(), &mut files);
    files
        .iter()
        .filter_map(|p| fs::read_to_string(p).ok())
        .collect()
}

/// Concatenate up to `max_fragments` random [`FRAGMENTS`] into one source string.
fn random_soup(rng: &mut Rng, max_fragments: usize) -> String {
    let n = rng.below(max_fragments) + 1;
    let mut s = String::new();
    for _ in 0..n {
        s.push_str(rng.pick(FRAGMENTS));
    }
    s
}

/// Apply a handful of char-level edits to a seed, keeping the result valid UTF-8.
fn mutate(rng: &mut Rng, seed: &str) -> String {
    let mut chars: Vec<char> = seed.chars().collect();
    let edits = rng.below(16) + 1;
    for _ in 0..edits {
        if chars.is_empty() {
            chars.push(
                *rng.pick(FRAGMENTS)
                    .chars()
                    .collect::<Vec<_>>()
                    .first()
                    .unwrap_or(&'x'),
            );
            continue;
        }
        match rng.below(5) {
            0 => {
                let i = rng.below(chars.len());
                chars.remove(i); // delete
            }
            1 => {
                let i = rng.below(chars.len());
                chars.insert(i, chars[i]); // duplicate
            }
            2 => {
                // insert a fragment's first char
                let frag = rng.pick(FRAGMENTS);
                if let Some(c) = frag.chars().next() {
                    let i = rng.below(chars.len() + 1);
                    chars.insert(i.min(chars.len()), c);
                }
            }
            3 => {
                let (a, b) = (rng.below(chars.len()), rng.below(chars.len()));
                chars.swap(a, b); // swap
            }
            _ => {
                chars.truncate(rng.below(chars.len() + 1)); // truncate
            }
        }
    }
    chars.into_iter().collect()
}

#[test]
fn parse_never_panics_on_corpus() {
    for seed in corpus_seeds() {
        let _ = gd_syntax::parse(&seed);
    }
}

#[test]
fn parse_never_panics_on_random_token_soup() {
    let mut rng = Rng::new(0xDEAD_BEEF_CAFE_F00D);
    for _ in 0..4000 {
        let src = random_soup(&mut rng, 250);
        let _ = gd_syntax::parse(&src);
    }
}

#[test]
fn parse_never_panics_on_mutated_corpus() {
    let mut rng = Rng::new(0x0123_4567_89AB_CDEF);
    let seeds = corpus_seeds();
    assert!(!seeds.is_empty(), "corpus seeds should be present");
    for seed in &seeds {
        for _ in 0..6 {
            let src = mutate(&mut rng, seed);
            let _ = gd_syntax::parse(&src);
        }
    }
}

#[test]
fn parse_never_panics_on_adversarial_edge_cases() {
    // Hand-picked shapes that have historically tripped recursive-descent parsers.
    let cases = [
        "",
        "\u{0}",
        "\u{feff}extends Node\n", // BOM
        &"(".repeat(100_000),
        &"[".repeat(100_000),
        &"Array[".repeat(100_000),
        &"\tif 0:\n".repeat(50_000),
        &"@".repeat(10_000),
        &"func f(".repeat(5_000),
        &"\\\n".repeat(10_000), // line continuations
        &"\"".repeat(10_000),   // unterminated strings
        &"match 0:\n\t".repeat(5_000),
        // A single line of many leading tabs/spaces stresses the indentation counters
        // (`column`/`indent_count`); they now saturate instead of overflowing on pathological runs.
        // (A true overflow needs ~500M+ chars — infeasible to allocate here — so this only covers
        // the accumulation path, which the one-tab-per-line cases above do not.)
        &"\t".repeat(200_000),
        &" ".repeat(200_000),
        &("\t".repeat(100_000) + "pass\n"),
    ];
    for case in cases {
        let _ = gd_syntax::parse(case);
    }
}
