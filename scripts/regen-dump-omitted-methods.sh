#!/usr/bin/env bash
# regen-dump-omitted-methods.sh — regenerate gd_types' DUMP_OMITTED_NATIVE_METHODS table (#172).
#
# Re-derives the `(class, method, is_virtual)` triples ClassDB resolves but the vendored stock dump
# omits, from a live Godot binary + the vendored stock dump. The table is hand-vendored in
# `crates/gd_types/src/native_db.rs`; this makes it reproducible so it can't silently drift when the
# stock dump is bumped to a new Godot version. NOT a CI step — it needs the Godot binary.
#
# Usage:
#     scripts/regen-dump-omitted-methods.sh [godot-binary]
#
# `godot-binary` defaults to `godot` on PATH (pass an absolute path or a PATH name, not a
# CWD-relative one). The binary's version MUST match the vendored stock
# dump's version (the .gd refuses on a mismatch). Output (the paste-ready table body) goes to STDOUT;
# diagnostics go to STDERR. Paste the rows between the `&[` / `];` markers of
# DUMP_OMITTED_NATIVE_METHODS, then run `cargo fmt --all` (rustfmt re-wraps the few >100-col rows).
set -euo pipefail

GODOT="${1:-godot}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
STOCK_GZ="$REPO_ROOT/crates/gd_server/assets/extension_api_4.6.3_stock.min.json.gz"
GD_TOOL="$SCRIPT_DIR/regen-dump-omitted-methods.gd"

if ! command -v "$GODOT" >/dev/null 2>&1 && [[ ! -x "$GODOT" ]]; then
	echo "error: Godot binary not found: $GODOT" >&2
	exit 1
fi
if [[ ! -f "$STOCK_GZ" ]]; then
	echo "error: vendored stock dump not found: $STOCK_GZ" >&2
	exit 1
fi

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT
DUMP_JSON="$WORKDIR/extension_api.json"
OUT_TXT="$WORKDIR/dump_omitted_methods.txt"

gunzip -c "$STOCK_GZ" >"$DUMP_JSON"

# `--headless` so no window/GL; the .gd writes the table to OUT_TXT (NOT stdout) so Godot's boot
# banner never contaminates it. Do NOT pass `--quiet` — it also suppresses the script's diagnostics.
# Let the OUTPUT ARTIFACT decide success, not Godot's exit code: some Godot builds return non-zero
# even on a clean headless `--script` exit, so `set -e` here would abort on a perfectly good run.
set +e
"$GODOT" --headless --script "$GD_TOOL" -- "$DUMP_JSON" "$OUT_TXT" >&2
set -e

if [[ ! -s "$OUT_TXT" ]]; then
	echo "error: regeneration produced no rows (see the Godot diagnostics above)" >&2
	exit 1
fi

# The paste-ready table body to stdout.
cat "$OUT_TXT"
