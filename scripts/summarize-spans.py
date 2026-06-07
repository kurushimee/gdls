#!/usr/bin/env python3
"""M5 WP-P1 — summarise the JSONL trace produced by `gdls` with GDLS_LOG_FORMAT=json
into a per-span percentile table and an RSS peak.

Reads the newline-delimited JSON the WP-O1 / WP-P1 logger writes to stderr (redirected
by the caller into a file), groups close events by span name — splitting `handle_request`
events by their `method` field — and prints a percentile table (count, p50, p95, p99, max)
of `elapsed_us` per group. The RSS section lists peak / baseline observed across all
`target=rss` events.

The table is the operator-facing artefact fed into `bench/budget.toml` by WP-P5. Numbers
are formatted in milliseconds (1 decimal) for the latency buckets and in MB (1 decimal)
for the memory bucket — matching the budget file's units so a copy-paste round-trip is
trivial.

Capture a trace first with:

    export GDLS_LOG_FORMAT=json
    export GDLS_LOG="info,handle_request=info,analyze=info,cold_index=info,reconcile=info,watcher_event=info,rss=info"
    gdls 2> target/m5-p1-calib-YYYY-MM-DD.jsonl
    # (LSP client driving on stdin / stdout.)

Usage:
    scripts/summarize-spans.py [--path TRACE.jsonl] [--tsv]

With no --path, the most recent `target/m5-p1-calib-*.jsonl` under the current working
directory is used.
"""

from __future__ import annotations

import argparse
import glob
import json
import math
import os
import sys


def percentile(values, p):
    """Linear-interpolated percentile on the sorted values; matches
    `numpy.percentile(.., interpolation='linear')` (the convention rust-analyzer's perf
    script uses). Returns 0.0 on empty input so the caller can render `n/a` from context."""
    arr = sorted(float(v) for v in values)
    if not arr:
        return 0.0
    if len(arr) == 1:
        return arr[0]
    rank = (p / 100.0) * (len(arr) - 1)
    lo = math.floor(rank)
    hi = math.ceil(rank)
    if lo == hi:
        return arr[lo]
    frac = rank - lo
    return arr[lo] * (1.0 - frac) + arr[hi] * frac


def resolve_path(path):
    if path:
        if not os.path.isfile(path):
            sys.exit(f"Trace file not found: {path}")
        return path
    candidates = glob.glob(os.path.join("target", "m5-p1-calib-*.jsonl"))
    if not candidates:
        sys.exit(
            "No --path supplied and no target/m5-p1-calib-*.jsonl files found under "
            f"{os.getcwd()}. Capture a trace first with GDLS_LOG_FORMAT=json and stderr "
            "redirected to that path."
        )
    return max(candidates, key=os.path.getmtime)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--path", help="path to the JSONL trace file")
    parser.add_argument("--tsv", action="store_true", help="emit machine-readable TSV instead of the human table")
    args = parser.parse_args()

    path = resolve_path(args.path)

    span_buckets = {}        # name -> [elapsed_us]
    request_buckets = {}     # "handle_request.<method>" -> [elapsed_us]
    reconcile_fields = ["added", "modified", "removed", "walked", "walk_errors",
                        "skipped_unreadable", "skipped_non_utf8"]
    reconcile_totals = {f: 0 for f in reconcile_fields}
    reconcile_runs = 0
    rss_samples = 0
    rss_baseline = 0
    rss_peak = 0
    cold_index = {"file_count": 0, "elapsed_us": 0.0}
    bad_lines = 0

    with open(path, encoding="utf-8") as fh:
        for line in fh:
            trim = line.strip()
            # tracing-subscriber JSON output is one object per line, but an operator may have
            # appended non-JSON noise; skip lines that don't start with `{`.
            if not trim or not trim.startswith("{"):
                continue
            try:
                event = json.loads(trim)
            except json.JSONDecodeError:
                bad_lines += 1
                continue

            target = event.get("target", "")
            fields = event.get("fields") or {}
            span = event.get("span") or {}

            # --- RSS samples: target=rss, fields.bytes / peak_bytes / baseline_bytes ---
            if target == "rss" and "bytes" in fields:
                rss_samples += 1
                if "peak_bytes" in fields:
                    rss_peak = max(rss_peak, int(fields["peak_bytes"]))
                if "baseline_bytes" in fields and rss_baseline == 0:
                    rss_baseline = int(fields["baseline_bytes"])
                continue

            # --- Span close events: fields.message="close", span.name + recorded fields ---
            if fields.get("message") != "close":
                continue
            name = span.get("name")
            if not name:
                continue
            elapsed = float(span.get("elapsed_us", 0.0))

            if name == "cold_index":
                cold_index["elapsed_us"] = elapsed
                cold_index["file_count"] = int(span.get("file_count", 0))
            elif name == "handle_request":
                method = span.get("method", "<unknown>")
                request_buckets.setdefault(f"handle_request.{method}", []).append(elapsed)
            elif name == "reconcile":
                reconcile_runs += 1
                for f in reconcile_fields:
                    if f in span:
                        reconcile_totals[f] += int(span[f])
                span_buckets.setdefault(name, []).append(elapsed)
            else:
                span_buckets.setdefault(name, []).append(elapsed)

    def stats(samples):
        return (
            len(samples),
            percentile(samples, 50) / 1000.0,
            percentile(samples, 95) / 1000.0,
            percentile(samples, 99) / 1000.0,
            (max(samples) if samples else 0.0) / 1000.0,
        )

    if args.tsv:
        print("group\tcount\tp50_ms\tp95_ms\tp99_ms\tmax_ms")
        if cold_index["elapsed_us"] > 0:
            ms = cold_index["elapsed_us"] / 1000.0
            print(f"cold_index\t1\t{ms:.1f}\t{ms:.1f}\t{ms:.1f}\t{ms:.1f}")
        for name in sorted(span_buckets):
            n, p50, p95, p99, mx = stats(span_buckets[name])
            print(f"{name}\t{n}\t{p50:.1f}\t{p95:.1f}\t{p99:.1f}\t{mx:.1f}")
        for key in sorted(request_buckets):
            n, p50, p95, p99, mx = stats(request_buckets[key])
            print(f"{key}\t{n}\t{p50:.1f}\t{p95:.1f}\t{p99:.1f}\t{mx:.1f}")
        print(f"rss.peak_mb\t1\t{rss_peak / 1048576.0:.1f}\t{rss_peak / 1048576.0:.1f}"
              f"\t{rss_peak / 1048576.0:.1f}\t{rss_peak / 1048576.0:.1f}")
        print(f"rss.baseline_mb\t1\t{rss_baseline / 1048576.0:.1f}\t{rss_baseline / 1048576.0:.1f}"
              f"\t{rss_baseline / 1048576.0:.1f}\t{rss_baseline / 1048576.0:.1f}")
        return

    print()
    print("M5 WP-P1 — span summary")
    print(f"Source: {path}")
    print()
    print("Group                             count   p50(ms)   p95(ms)   p99(ms)   max(ms)")
    print("--------------------------------- ----- --------- --------- --------- ---------")

    if cold_index["elapsed_us"] > 0:
        ms = cold_index["elapsed_us"] / 1000.0
        print(f"{'cold_index':<33} {1:>5} {ms:>9.1f} {ms:>9.1f} {ms:>9.1f} {ms:>9.1f}")
        print(f"  (cold-index file_count = {cold_index['file_count']})")

    for name in sorted(span_buckets):
        n, p50, p95, p99, mx = stats(span_buckets[name])
        print(f"{name:<33} {n:>5} {p50:>9.1f} {p95:>9.1f} {p99:>9.1f} {mx:>9.1f}")
    for key in sorted(request_buckets):
        n, p50, p95, p99, mx = stats(request_buckets[key])
        print(f"{key:<33} {n:>5} {p50:>9.1f} {p95:>9.1f} {p99:>9.1f} {mx:>9.1f}")

    print()
    print("RSS")
    print("---")
    print(f"samples observed:  {rss_samples}")
    print(f"baseline_mb:       {rss_baseline / 1048576.0:>8.1f}")
    print(f"peak_mb:           {rss_peak / 1048576.0:>8.1f}")

    if reconcile_runs > 0:
        print()
        print(f"Reconcile counters (summed across {reconcile_runs} run(s))")
        print("------------------------------------------------")
        for f in reconcile_fields:
            print(f"  {f:<22} {reconcile_totals[f]}")

    if bad_lines > 0:
        print(f"WARNING: skipped {bad_lines} unparseable line(s) in {path}", file=sys.stderr)

    peak_mb = rss_peak / 1048576.0
    print()
    print("Suggested bench/budget.toml values (WP-P5):")
    print(f"  peak_rss_mb  = {peak_mb:.0f}")
    print(f"  soft_cap_mb  = {peak_mb * 2:.0f}   # peak × 2")
    print(f"  hard_cap_mb  = {peak_mb * 4:.0f}   # peak × 4")


if __name__ == "__main__":
    main()
