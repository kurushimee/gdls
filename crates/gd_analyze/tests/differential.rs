//! WP-D1 (M5) differential-check harness — runs gdls and the godot binary against
//! the in-repo `differential_sample/` corpus and compares their diagnostic code-sets per fixture.
//!
//! Catches a silent ratcheting divergence between our port and Godot, which the per-file
//! conformance corpus (`tests/conformance/`) can miss: the conformance corpus is a fixed-point
//! snapshot, while this harness re-asks the live Godot binary the same question every run.
//!
//! **Local-only, never in CI.** A local Godot checkout is read-only inspection (CLAUDE.md:
//! "Godot is the source of truth"); CI workflows must not clone, build, or invoke any Godot
//! artefact. The pre-built `godot` already on `PATH` is the *only* Godot binary this harness ever
//! touches, and only locally. The test is `#[ignore]`-d so `cargo test --workspace` stays green
//! and never tries to spawn anything from PATH.
//!
//! **Invocation**:
//! ```text
//! cargo test -p gd_analyze --test differential -- --ignored --nocapture
//! ```
//!
//! **Discovery**: `$GDLS_GODOT_BINARY` (absolute path) wins if set; otherwise `which::which("godot")`
//! is probed. If neither resolves, the test prints a `differential: godot not on PATH; skipping`
//! line on stderr and returns `Ok(())` — a no-op on machines without the binary.
//!
//! **Mechanism**: for each `NN_*.gd` fixture in the corpus, the harness
//!   1. invokes `<godot> --headless --path <corpus-dir> --check-only --quit --script res://<file>`
//!      (Godot 4.6.3 has no `--script-debug` flag — the M5 plan's draft cited it but the real flag
//!      pair is `--check-only --script`, verified against `godot --help`. `--path` pins the
//!      corpus's `project.godot` as the resource root so `res://` preloads / sibling scripts
//!      resolve deterministically regardless of the cargo runner's CWD);
//!   2. captures stderr, strips line-numbers and trailing whitespace, and extracts the set of
//!      diagnostic codes Godot emitted (`SCRIPT ERROR:` → `"error"`, `WARNING: <CODE>:` → the
//!      upper-case CODE; bootstrap noise like the bare `ERROR: Condition "!configured"...` lines
//!      that fire before the string pool is initialised is filtered out — see [`godot_diag_codes`]);
//!   3. runs `gd_analyze::analyze` on the same source with the canonical native DB fixture
//!      (`trimmed_api.json`) so native-class member access on fixtures 01/05/09 doesn't trivially
//!      diverge, and an empty cross-file environment (single-file analyze; the harness measures
//!      per-file divergence, not whole-project depth);
//!   4. computes Jaccard similarity `|A ∩ B| / |A ∪ B|` on the two code-sets (two empty sets are
//!      defined as `1.0` — perfect agreement on emitting nothing).
//!
//! **Pass/fail**: the aggregate is mean per-fixture Jaccard; the test asserts mean ≥ threshold
//! (default `0.85`, overridable via `$GDLS_DIFFERENTIAL_THRESHOLD`). On mismatch the per-fixture
//! diff table is printed so the gap is visible. Post-Phase-E the WP-F* parser fixtures (`11_*`
//! `12_*` `13_*` `14_*`) all agree at 1.0; the only standing sub-1.0 fixtures are the cross-file
//! identifier cases (`02_autoload_reference`, `07_const_external_member`), where Godot's
//! single-file `--check-only` errors on an undeclared autoload / external `class_name` while gdls
//! deliberately stays silent on unresolved uppercase identifiers (the "unknown stays dynamic /
//! never false-positive" policy, docs/00). A NEW sub-1.0 fixture is a real regression.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use gd_analyze::{NoCrossFile, StrictSettings, WarnPolicy};
use gd_project::{FileId, WarningConfig};
use gd_types::NativeDb;

/// The shared native DB fixture (`crates/gd_types/tests/fixtures/trimmed_api.json`, 1203 classes —
/// the same one the analyzer conformance harness loads). Loaded once per test process.
fn native_db() -> &'static NativeDb {
    static DB: OnceLock<NativeDb> = OnceLock::new();
    DB.get_or_init(|| {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../gd_types/tests/fixtures/trimmed_api.json");
        NativeDb::load(path.to_str().expect("utf-8 path")).unwrap_or_else(|e| {
            panic!(
                "differential harness could not load native DB fixture at {}: {e}\n\
                 (the file is committed; if it's missing, the analyzer conformance harness would \
                 also be broken — run that first)",
                path.display()
            )
        })
    })
}

/// Resolve the godot binary location: `$GDLS_GODOT_BINARY` wins absolute-path-style; otherwise
/// `which::which("godot")` walks `PATH`. Returns `None` if neither produces a callable binary,
/// at which point the test no-ops with a skip message.
fn resolve_godot_binary() -> Option<PathBuf> {
    if let Some(env) = std::env::var_os("GDLS_GODOT_BINARY") {
        let candidate = PathBuf::from(env);
        if candidate.is_file() {
            return Some(candidate);
        }
        eprintln!(
            "differential: GDLS_GODOT_BINARY={} is set but does not point at a file; falling back \
             to PATH lookup",
            candidate.display()
        );
    }
    // `which::which` handles `.exe` resolution on Windows and PATH parsing on every platform.
    which::which("godot").ok()
}

/// Resolve the `$GDLS_DIFFERENTIAL_THRESHOLD` override (a float in `[0.0, 1.0]`); on parse failure
/// or absence, fall back to the M5 plan's `0.85` default.
fn threshold() -> f64 {
    std::env::var("GDLS_DIFFERENTIAL_THRESHOLD")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|f| (0.0..=1.0).contains(f))
        .unwrap_or(0.85)
}

/// Walk the corpus and return only the numbered fixtures (`NN_short_name.gd`). Helper files
/// (`_autoload.gd`, `_helper.gd`) are intentionally excluded — they exist to give Godot
/// something to resolve for the cross-file fixtures, not to be diffed themselves.
fn collect_fixtures(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !path.is_file() {
            continue;
        }
        if !name.ends_with(".gd") {
            continue;
        }
        // Numbered fixtures only; helpers (`_helper.gd`, `_autoload.gd`) get skipped.
        let head: String = name.chars().take(2).collect();
        if head.chars().all(|c| c.is_ascii_digit()) {
            out.push(path);
        }
    }
    out.sort();
    out
}

/// Extract the set of diagnostic codes from a `gd_analyze::AnalysisResult`. Mirrors the existing
/// (M3 WP-I) helper: errors carry the literal `"error"` code (every `push_error` routes through
/// the same code), warnings carry their Godot-canonical name (uppercase). Set semantics so that
/// the order in which diagnostics happen to be emitted doesn't perturb agreement.
fn gdls_diag_codes(source: &str, script_path: &str) -> BTreeSet<String> {
    let parsed = gd_syntax::parse(source);
    // Parser-phase diagnostics (syntax / annotation / tokenizer errors — e.g. the Phase E
    // duplicate-`@icon`, duplicate-`@tool`, and identifier-similar-to-keyword diagnostics) are
    // emitted by `gd_syntax`, NOT the analyzer, and Godot prints them as `SCRIPT ERROR: Parse
    // Error: …` (mapped to the literal `"error"` code by `godot_diag_codes`).
    //
    // Crucially, Godot's `--check-only` ABORTS on a parse error: it prints the parse error and
    // `Failed to load script … Parse error`, and never runs the analyzer / warning pass. So when
    // gdls's parser produced any diagnostic, the comparable code-set is exactly `{"error"}` —
    // running the analyzer on the partial tree here would add warnings Godot never reaches,
    // manufacturing a spurious divergence (this mirrors the conformance harness's
    // GDTEST_ANALYZER_ERROR warning-strip, one phase earlier).
    if !parsed.diagnostics.is_empty() {
        return BTreeSet::from(["error".to_owned()]);
    }
    let xfile = NoCrossFile;
    let policy = WarnPolicy::build(&WarningConfig::default(), &StrictSettings::default());
    let result = gd_analyze::analyze(
        &parsed.tree,
        Some(FileId::new(1)),
        script_path,
        native_db(),
        &xfile,
        &policy,
    );
    result
        .diagnostics
        .iter()
        .map(|d| d.code().to_owned())
        .collect()
}

/// Extract Godot's diagnostic codes from a `godot --check-only` invocation's stderr.
///
/// Format quirks the parser must handle:
/// * **`SCRIPT ERROR:` prefix** is the analyzer error tag (analyzer.cpp → `_print_error`), mapped
///   to the literal `"error"` code so it matches gdls's `Diagnostic::code()`.
/// * **`WARNING: <CODE>: …`** is the analyzer warning format; the CODE token (before the second
///   `:`) is canonicalised to upper-case to match gdls's warning-name code. **Only kept** when the
///   immediately-following `at:` continuation references either a `.gd` file (real analyzer-source
///   location) or `gdscript_*.cpp` (analyzer/parser source). Godot emits engine-level WARNINGs
///   like `WARNING: Mismatch argument name count for virtual method: '<class>::<method>'.` from
///   `ClassDB::add_virtual_method` on every cold start — those are GDExtension-class registration
///   noise, not analyzer warnings, and their `at:` references `core/object/class_db.cpp`. Without
///   this filter every fixture's Godot code-set would be polluted with the same engine warning.
/// * **Bare `ERROR: Condition "!configured" …`** lines fire before the string pool is initialised,
///   well before `--check-only` short-circuits — they're engine bootstrap noise, not analyzer
///   output, and were responsible for the M3 WP-I harness's "every fixture's Godot code-set =
///   {error}" bug. Filtered out by requiring the `SCRIPT ERROR:` prefix for the `error` mapping.
/// * **ANSI colour escapes** from Godot's `print_line` colourised output are stripped before
///   prefix matching so the parser sees the raw text.
fn godot_diag_codes(binary: &Path, gd: &Path) -> BTreeSet<String> {
    // Pin the project root so Godot resolves `res://` deterministically. Without `--path`, Godot
    // anchors `res://` to its CWD (the cargo test runner's dir), so a cross-file fixture
    // (`preload("res://_helper.gd")`, an autoload, …) randomly fails to resolve depending on where
    // the test was launched. The differential sample ships a `project.godot`; pointing `--path` at
    // the fixture's directory and passing the script as a `res://`-relative path makes Godot
    // see the same project the gdls side's cross-file resolution would (Godot still can't load
    // autoloads under `--check-only`, so the autoload/external-const fixtures stay divergent by
    // gdls's deliberate "unknown stays dynamic" policy — accepted within the Jaccard floor).
    let dir = gd.parent().map(Path::to_path_buf).unwrap_or_default();
    let res_script = format!(
        "res://{}",
        gd.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default()
    );
    let output = Command::new(binary)
        .args([
            OsString::from("--headless"),
            OsString::from("--path"),
            dir.into_os_string(),
            OsString::from("--check-only"),
            OsString::from("--quit"),
            OsString::from("--script"),
            OsString::from(res_script),
        ])
        .output();
    let Ok(output) = output else {
        return BTreeSet::new();
    };
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lines: Vec<String> = stderr.lines().map(strip_ansi).collect();
    let mut codes = BTreeSet::new();
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("SCRIPT ERROR:") {
            codes.insert("error".to_owned());
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("WARNING:") else {
            continue;
        };
        // The next line (if present) is the `   at: <symbol> (<file>:<line>)` continuation. If it
        // references engine source (anything other than `.gd` or `gdscript_*.cpp`) the WARNING is
        // engine-internal noise (ClassDB::add_virtual_method etc.) and gets dropped.
        let at_line = lines.get(idx + 1).map(String::as_str).unwrap_or("");
        let at_trim = at_line.trim_start();
        let is_analyzer_warning = at_trim.starts_with("at:")
            && (at_trim.contains(".gd")
                || at_trim.contains("gdscript_analyzer.cpp")
                || at_trim.contains("gdscript_parser.cpp"));
        if !is_analyzer_warning {
            continue;
        }
        if let Some(code_part) = rest.trim_start().split(':').next() {
            let code = code_part.trim();
            if !code.is_empty() {
                codes.insert(code.to_ascii_uppercase());
            }
        }
    }
    codes
}

/// Strip ANSI CSI sequences (`ESC [ ... letter`) from a line so the prefix-matching parser sees
/// the raw text. Godot's `print_line` colourises stderr with sequences like `\x1b[38;5;39m`;
/// without this strip, `trimmed.starts_with("SCRIPT ERROR:")` would miss a coloured line.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{001b}' && chars.peek() == Some(&'[') {
            chars.next();
            while let Some(&next) = chars.peek() {
                chars.next();
                if next.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// Jaccard similarity `|A ∩ B| / |A ∪ B|`. Defined as `1.0` when both sets are empty (perfect
/// agreement on emitting nothing — the alternative `0/0 = NaN` would poison the aggregate mean).
fn jaccard(a: &BTreeSet<String>, b: &BTreeSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    inter / union
}

#[test]
#[ignore = "local-only differential; run with `cargo test -p gd_analyze --test differential -- --ignored --nocapture`"]
fn godot_differential_meets_jaccard_floor() {
    let Some(godot) = resolve_godot_binary() else {
        eprintln!(
            "differential: godot not on PATH; skipping (set $GDLS_GODOT_BINARY=<abs path> to point \
             at a specific build, or install the godot binary on PATH)"
        );
        return;
    };

    let corpus = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/differential_sample");
    if !corpus.is_dir() {
        panic!(
            "differential: corpus directory missing at {} — WP-D2 commit?",
            corpus.display()
        );
    }
    let fixtures = collect_fixtures(&corpus);
    assert!(
        !fixtures.is_empty(),
        "differential: no NN_*.gd fixtures found under {}",
        corpus.display()
    );

    eprintln!(
        "differential: godot={} | corpus={} | fixtures={} | threshold={:.4}",
        godot.display(),
        corpus.display(),
        fixtures.len(),
        threshold()
    );

    let mut rows: Vec<(PathBuf, BTreeSet<String>, BTreeSet<String>, f64)> =
        Vec::with_capacity(fixtures.len());
    let mut sum_jaccard = 0.0;

    for gd in &fixtures {
        let Ok(source) = fs::read_to_string(gd) else {
            eprintln!("  ✘ {}: unreadable, skipping (counts as 0.0)", gd.display());
            rows.push((gd.clone(), BTreeSet::new(), BTreeSet::new(), 0.0));
            continue;
        };
        let script_path = gd.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let ours = gdls_diag_codes(&source, script_path);
        let theirs = godot_diag_codes(&godot, gd);
        let j = jaccard(&ours, &theirs);
        sum_jaccard += j;
        rows.push((gd.clone(), ours, theirs, j));
    }

    let mean = sum_jaccard / fixtures.len() as f64;
    eprintln!(
        "differential: mean Jaccard = {:.4} across {} fixture(s) (floor {:.4})",
        mean,
        fixtures.len(),
        threshold()
    );

    // Print the per-fixture table sorted by Jaccard ascending so the worst divergences land at the
    // top — easier to triage when only the first screenful of `--nocapture` survives.
    let mut sorted = rows.clone();
    sorted.sort_by(|a, b| a.3.partial_cmp(&b.3).unwrap_or(std::cmp::Ordering::Equal));
    for (gd, ours, theirs, j) in &sorted {
        let name = gd.file_name().and_then(|n| n.to_str()).unwrap_or("?");
        let marker = if (*j - 1.0).abs() < f64::EPSILON {
            "✓"
        } else {
            "✘"
        };
        eprintln!(
            "  {marker} {name}  jaccard={j:.4}\n      gdls: {ours:?}\n      godot: {theirs:?}"
        );
    }

    assert!(
        mean + 1e-9 >= threshold(),
        "differential mean Jaccard {mean:.4} fell below floor {:.4} — see per-fixture table \
         above. The known sub-1.0 fixtures are the cross-file-identifier cases \
         (`02_autoload_reference`, `07_const_external_member`): Godot's single-file \
         `--check-only` errors on an undeclared autoload / external `class_name`, while gdls \
         deliberately stays silent on unresolved uppercase identifiers (the \"unknown stays \
         dynamic / never false-positive\" policy, docs/00). A NEW sub-1.0 fixture is a real \
         regression to investigate. To override locally: \
         GDLS_DIFFERENTIAL_THRESHOLD=<f64> (bash).",
        threshold()
    );
}
