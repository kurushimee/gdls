//! `gdls` — a standalone GDScript language server for Godot, speaking LSP to Claude Code over
//! stdio with no Godot process at runtime. One binary serves every release from
//! [`Dialect::OLDEST`](gd_syntax::Dialect::OLDEST) to
//! [`Dialect::NEWEST`](gd_syntax::Dialect::NEWEST), picked per project.
//!
//! The binary is a thin wrapper; all logic lives in the `gd_server` library so it can be
//! integration-tested over an in-memory connection.
//!
//! The default invocation runs the LSP server on stdio. The `diagnose` subcommand (M4 WP-T3)
//! is the "post-suspend / remote-FS recovery" and "is-my-index-consistent" tool:
//!   - `--reconcile` walks `res://**/*.gd` against the index and prints a `ReconciliationReport`
//!     to stderr — verifies watcher behaviour after wake-from-suspend or remote-FS hiccups without
//!     starting a session.
//!   - `--path-audit` runs `Index::verify()` over the loaded index and reports any cross-table
//!     identity violation (dead FileId, dangling `class_name`, `paths`↔`ids` / DepGraph asymmetry).
//!
//! Either flag (or both) may be given. The CLI initializes the `log` backend so per-file warnings
//! (unreadable scripts, walk errors, IndexMutation quarantines) reach the operator on stderr, and
//! exits nonzero when the walk hit any error or the audit found a violation, so wrapper scripts can
//! detect "found nothing because we couldn't read the tree / the index is inconsistent" vs "clean".

use std::path::PathBuf;

use anyhow::Result;
use camino::Utf8PathBuf;

use gd_server::bench::{BenchRecorder, DEFAULT_TRACE_CAPACITY};
use gd_server::config::InitializationOptions;
use gd_server::Workspace;
use gd_syntax::Dialect;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    // Hand-rolled argv dispatch (no clap dependency). Subcommands so far:
    //   - `gdls --version` / `--help` (terminal probes; print to stdout and exit before any LSP
    //     traffic, so the "stdout is the LSP wire" rule doesn't apply — the server never starts)
    //   - `gdls diagnose --reconcile [--root <path>]` (M4 WP-T3)
    //   - `gdls bench --record <path>` / `--replay <path>` (M5 WP-P3)
    if args.len() >= 2 {
        match args[1].as_str() {
            "--version" | "-V" => {
                println!("gdls {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "--help" | "-h" => {
                print_usage();
                return Ok(());
            }
            "diagnose" => return diagnose(&args[2..]),
            "bench" => return bench_cli(&args[2..]),
            _ => {}
        }
    }
    gd_server::run()
}

/// Top-level `gdls --help`. The default (no-subcommand) invocation speaks LSP over stdio, so an
/// operator running `gdls` by hand expecting usage text gets it via this flag instead of a server
/// that silently waits on a `Content-Length` header.
fn print_usage() {
    println!(
        "gdls {} — a standalone GDScript language server for Godot {} to {}\n\
         \n\
         USAGE:\n\
         \x20   gdls                                  Run the LSP server on stdio (default)\n\
         \x20   gdls --version | -V                   Print version and exit\n\
         \x20   gdls --help | -h                      Print this help and exit\n\
         \x20   gdls diagnose (--reconcile | --path-audit) [--root <path>]\n\
         \x20                                         One-shot index health check (no session)\n\
         \x20   gdls bench --record <path> | --replay <path>\n\
         \x20                                         Record / replay a request trace (local repro)\n\
         \n\
         The server is normally launched by an LSP client (the Claude Code plugin lives at\n\
         github.com/kurushimee/gdls-plugin). Logs go to stderr; set GDLS_LOG (e.g.\n\
         GDLS_LOG=info) to tune verbosity and GDLS_TRACE for the hierarchical span profiler.",
        env!("CARGO_PKG_VERSION"),
        Dialect::OLDEST.as_str(),
        Dialect::NEWEST.as_str()
    );
}

fn diagnose(args: &[String]) -> Result<()> {
    let do_reconcile = args.iter().any(|a| a == "--reconcile");
    let do_audit = args.iter().any(|a| a == "--path-audit");
    if !do_reconcile && !do_audit {
        // At least one of the two is required, so the usage line must not bracket both as
        // optional — it contradicted the exit-2 it was printed alongside (#309).
        eprintln!("usage: gdls diagnose (--reconcile | --path-audit) [--root <path>]");
        std::process::exit(2);
    }
    // Initialize the log backend so warn/error from Workspace::load / reconcile / IndexMutation
    // reaches the operator on stderr. Without this, every per-file warning is logged to a
    // sink with no consumer, and a diagnostic run masquerades as clean when files are
    // unreadable or invariants violated mid-walk.
    gd_server::logging::init();

    let root = parse_root_flag(args).unwrap_or_else(|| Utf8PathBuf::from("."));
    let options = InitializationOptions::default();
    eprintln!("loading workspace at {root}...");
    let mut workspace = Workspace::load(&root, &options);
    eprintln!("loaded {} script(s)", workspace.index.file_count());
    // A stock-surface run would otherwise inherit the resolve ladder's advice ("set
    // godotBinaryPath or GDLS_GODOT for an exact dump") unqualified — but diagnose never
    // dumps; the background dump is the LSP session's job (api_dump::spawn_background_dump,
    // issue #25). Say so, so an operator doesn't set the env var and re-run diagnose
    // expecting a different native surface.
    if workspace.native.provenance() == gd_types::ApiProvenance::Generic
        && options.extension_api_path.is_none()
    {
        eprintln!(
            "note: diagnose never generates the native API dump (that is the LSP session's \
             background job); this run used the embedded stock surface. Open the project in \
             any LSP session once, or set godotBinaryPath / GDLS_GODOT, to produce an exact dump."
        );
    }

    let mut failed = false;

    if do_reconcile {
        eprintln!("reconciling against disk...");
        // `gdls diagnose` is a one-shot CLI, not an LSP session, so it has no open buffers —
        // reconcile against disk with an empty open-paths set (disk is authoritative here).
        let report = workspace.reconcile(&rustc_hash::FxHashSet::default());
        eprintln!(
            "cold_index_reconciled added={} modified={} removed={} walked={} \
             walk_errors={} skipped_unreadable={} skipped_non_utf8={}",
            report.added,
            report.modified,
            report.removed,
            report.walked,
            report.walk_errors,
            report.skipped_unreadable,
            report.skipped_non_utf8
        );
        if report.had_errors() {
            eprintln!(
                "WARNING: reconcile encountered {} walk error(s), {} unreadable file(s), \
                 {} non-UTF-8 path(s); check stderr above for per-file detail. Exiting nonzero.",
                report.walk_errors, report.skipped_unreadable, report.skipped_non_utf8
            );
            failed = true;
        } else {
            // Reconcile succeeded — persist a fresh warm-start cache so the next launch
            // (or a subsequent `gdls diagnose` after a wake-from-suspend) can avoid a full
            // cold walk. Fire-and-forget (log-only on failure, matching server.rs's wiring).
            workspace.save_cache();
        }
    }

    if do_audit {
        eprintln!(
            "path-audit: verifying index identity (paths<->ids symmetry, dead FileIds, dangling \
             class_names, DepGraph + name-reference inverses)..."
        );
        match workspace.index.verify() {
            Ok(()) => eprintln!(
                "path-audit: OK - {} file(s), {} dirty; all index invariants hold",
                workspace.index.file_count(),
                workspace.index.dirty_count()
            ),
            Err(violations) => {
                eprintln!(
                    "path-audit: {} index-invariant violation(s) - the index has inconsistent \
                     identity state:",
                    violations.len()
                );
                for v in &violations {
                    eprintln!("  - {v:?}");
                }
                eprintln!(
                    "Exiting nonzero. A fresh `gdls` start rebuilds the index from disk; if this \
                     persists, file a report with the violation list above."
                );
                failed = true;
            }
        }
    }

    if failed {
        std::process::exit(1);
    }
    Ok(())
}

/// `gdls bench --record <path>` / `--replay <path>` — WP-P3 reproducer subcommand. Mirrors
/// `diagnose`'s usage-error convention (`exit(2)` on a missing flag value or a missing mode flag).
/// Named `bench_cli` (not `bench`) to avoid colliding with the built-in `#[bench]` attribute macro.
fn bench_cli(args: &[String]) -> Result<()> {
    let mut record = None;
    let mut replay = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--record" => {
                let Some(path) = iter.next() else {
                    eprintln!("error: --record requires a path argument");
                    std::process::exit(2);
                };
                record = Some(PathBuf::from(path));
            }
            "--replay" => {
                let Some(path) = iter.next() else {
                    eprintln!("error: --replay requires a path argument");
                    std::process::exit(2);
                };
                replay = Some(PathBuf::from(path));
            }
            _ => {
                eprintln!("error: unrecognised bench flag: {arg}");
                std::process::exit(2);
            }
        }
    }
    match (record, replay) {
        (Some(path), None) => {
            // Construct the recorder explicitly so we don't have to mutate the global env. The
            // ring-buffer size honours $GDLS_BENCH_RECORD_CAPACITY if set (same precedence as the
            // env-driven path in `gd_server::run`); otherwise the M5 plan's N=64 default.
            let capacity = std::env::var("GDLS_BENCH_RECORD_CAPACITY")
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .filter(|&n| n > 0)
                .unwrap_or(DEFAULT_TRACE_CAPACITY);
            let recorder = BenchRecorder::new(capacity, path);
            gd_server::run_with_recorder(Some(recorder))
        }
        (None, Some(path)) => {
            let metrics = gd_server::bench::replay_to_csv(&path, &mut std::io::stdout())?;
            eprintln!("bench replay: {} entries", metrics.len());
            Ok(())
        }
        (Some(_), Some(_)) => {
            eprintln!("error: --record and --replay are mutually exclusive");
            std::process::exit(2);
        }
        (None, None) => {
            eprintln!("usage: gdls bench --record <path> | --replay <path>");
            std::process::exit(2);
        }
    }
}

/// Parse `--root <path>`. A bare `--root` with no following value is a usage error, not a silent
/// fall-through to the `.` default: an operator who typos `gdls diagnose --reconcile
/// --root` would otherwise audit the CWD while believing they pointed at their project. Mirrors the
/// `exit(2)` usage-error convention the no-flags branch in `diagnose` uses.
fn parse_root_flag(args: &[String]) -> Option<Utf8PathBuf> {
    for (i, a) in args.iter().enumerate() {
        if a == "--root" {
            let Some(v) = args.get(i + 1) else {
                eprintln!("error: --root requires a path argument");
                std::process::exit(2);
            };
            return Some(Utf8PathBuf::from(v.as_str()));
        }
    }
    None
}
