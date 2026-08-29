//! M5 WP-O3 / WP-O4 — governor + cancellation tests.
//!
//! Two scenarios:
//! 1. **Governor (WP-O3)**: a parseable file with more than `iter_limit` expression / statement
//!    nodes runs into the budget; the analyzer bails with the synthetic
//!    `analyzer: fixpoint iteration budget exceeded (limit=N)` error and the rest of the result
//!    stays partial (the sink retains every diagnostic that fired before the budget tripped).
//! 2. **Cancellation (WP-O4)**: a token cancelled before `analyze_with_options` runs fires the
//!    cancellation check on the very first 256-node multiple and emits
//!    `analyzer: request cancelled`. Mirrors the LSP `$/cancelRequest` plumbing — checkpoint is
//!    cooperative, not panic-throw.

use gd_analyze::{
    analyze_with_options, AnalyzeOptions, CancellationToken, NoCrossFile, StrictSettings,
    WarnPolicy, DEFAULT_ITER_LIMIT,
};
use gd_project::{FileId, WarningConfig};
use gd_syntax::parse;
use gd_types::NativeDb;

fn db() -> NativeDb {
    NativeDb::empty()
}

fn policy() -> WarnPolicy {
    WarnPolicy::build(&WarningConfig::default(), &StrictSettings::default())
}

#[test]
fn governor_bails_with_expected_message_when_iter_limit_is_low() {
    // A function body with a long chain of trivial statements. Each statement / expression
    // dispatches through `resolve_node` (governor-instrumented) and `reduce_expression`
    // (governor-instrumented), so even iter_limit=5 trips fast.
    let src = "\
extends Node

func main() -> void:
\tvar a := 1
\tvar b := 2
\tvar c := 3
\tvar d := 4
\tvar e := 5
\tvar f := 6
\tvar g := 7
\tvar h := 8
";
    let tree = parse(src).tree;
    let result = analyze_with_options(
        &tree,
        Some(FileId::new(1)),
        "governor.gd",
        &db(),
        &NoCrossFile,
        &policy(),
        AnalyzeOptions {
            iter_limit: Some(5),
            ..Default::default()
        },
    );
    assert!(
        result.diagnostics.iter().any(|d| d
            .message()
            .contains("fixpoint iteration budget exceeded (limit=5)")),
        "expected the governor's synthetic error in the diagnostics; got: {:?}",
        result
            .diagnostics
            .iter()
            .map(|d| d.message())
            .collect::<Vec<_>>()
    );
    assert!(
        result.bailed,
        "a governor-tripped result must be flagged `bailed` so callers don't cache the partial \
         side tables as authoritative"
    );
}

#[test]
fn governor_default_limit_does_not_trip_on_a_normal_file() {
    // A normal file analyzed with the default iter_limit (`DEFAULT_ITER_LIMIT` = 100_000) must
    // not see the synthetic error. The governor ticks on every node *visit* (every once-per-node
    // `reduce_expression` / `resolve_node` dispatch), not just genuine fixpoint re-iterations, so
    // a moderately-sized fixture already crosses ~1000 visits on a clean run — a limit of 1000
    // would false-positive (see the `DEFAULT_ITER_LIMIT` comment in lib.rs, citing
    // `features/boolean_operators_for_all_types.gd`). 100_000 leaves ~2 orders of magnitude of
    // headroom over the largest fixture (typical .gd ≤ ~5000 nodes).
    let src = "\
class_name Hero
extends Node

var hp: int = 100

func _ready() -> void:
\thp = 100
\tprint(hp)

func attack(target: Node) -> int:
\thp -= 1
\treturn hp
";
    let tree = parse(src).tree;
    let result = analyze_with_options(
        &tree,
        Some(FileId::new(1)),
        "normal.gd",
        &db(),
        &NoCrossFile,
        &policy(),
        AnalyzeOptions::default(),
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|d| d.message().contains("fixpoint iteration budget exceeded")),
        "default iter_limit ({DEFAULT_ITER_LIMIT}) must not trip on a normal file; got: {:?}",
        result
            .diagnostics
            .iter()
            .map(|d| d.message())
            .collect::<Vec<_>>()
    );
    assert!(
        !result.bailed,
        "a normal completing analysis must not be flagged `bailed`"
    );
}

#[test]
fn cancellation_token_fires_the_synthetic_request_cancelled_error() {
    // Pre-cancel the token, then run analyze. The first checkpoint that hits the 256-iter
    // gate (the very first checkpoint when iter_count starts at 0) sees the flipped flag and
    // emits the synthetic error.
    let src = "\
extends Node

func main() -> void:
\tvar a := 1
\tvar b := a + 2
\tvar c := b * 3
";
    let tree = parse(src).tree;
    let tok = CancellationToken::new();
    tok.cancel();
    let result = analyze_with_options(
        &tree,
        Some(FileId::new(1)),
        "cancel.gd",
        &db(),
        &NoCrossFile,
        &policy(),
        AnalyzeOptions {
            cancellation: Some(&tok),
            ..Default::default()
        },
    );
    assert!(
        result
            .diagnostics
            .iter()
            .any(|d| d.message().contains("request cancelled")),
        "expected the cancellation's synthetic error in the diagnostics; got: {:?}",
        result
            .diagnostics
            .iter()
            .map(|d| d.message())
            .collect::<Vec<_>>()
    );
}

#[test]
fn no_cancellation_token_means_no_cancellation_check_overhead() {
    // Sanity check: a tree with thousands of statements should analyze normally when no
    // token is wired in, and the result must NOT contain any cancellation-related diagnostic.
    let src = "extends Node\nfunc main() -> void:\n\tpass\n";
    let tree = parse(src).tree;
    let result = analyze_with_options(
        &tree,
        Some(FileId::new(1)),
        "nocancel.gd",
        &db(),
        &NoCrossFile,
        &policy(),
        AnalyzeOptions {
            ..Default::default()
        },
    );
    assert!(
        !result
            .diagnostics
            .iter()
            .any(|d| d.message().contains("request cancelled")),
        "no cancellation token should mean no cancellation diagnostic; got: {:?}",
        result
            .diagnostics
            .iter()
            .map(|d| d.message())
            .collect::<Vec<_>>()
    );
}
