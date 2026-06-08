#!/usr/bin/env bash
# m6-acceptance/run.sh — M6 v1 capability acceptance runner.
#
# Drives lsp-poke.py against any GDScript project to verify:
#   1. Every exposed M6 LSP capability returns real data (not null/placeholder).
#   2. Warm-start cache is >=5x faster than cold start.
#
# Usage:
#   run.sh --project PROJECT_ROOT [--godot GODOT_BIN] \
#          [--api EXTENSION_API] [--session SESSION_JSON]
#
# Or via environment variables:
#   PROJECT_ROOT=... GODOT_BIN=... [EXTENSION_API=...] run.sh [--session ...]
#
# Flags always override environment variables when both are set.
#
# Required:
#   --project / PROJECT_ROOT   Absolute path to the Godot project root.
#
# One of these required unless --api / EXTENSION_API is supplied:
#   --godot   / GODOT_BIN      Path to the Godot binary.
#
# Optional:
#   --api     / EXTENSION_API  Path to an existing extension_api.json.
#                              If absent, dumped via GODOT_BIN.
#   --session SESSION_JSON     Session template (default: scripts/m6-acceptance/walk.json).
#                              Tokens __PROJECT_ROOT__ and __EXTENSION_API__ are substituted
#                              automatically. File-placeholder tokens (__CALLER_FILE__ etc.)
#                              must already be filled in the template, OR use a project-specific
#                              session from scripts/m6-acceptance/sessions/<project>.json.
#
# Output:
#   target/m6-acceptance/oss-report.json  (gitignored — C2 adds the ignore glob)
#
# Exit codes:
#   0 — all capabilities pass AND warm start >=5x faster than cold
#   1 — any capability failure, speedup below threshold, or usage error

set -euo pipefail

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

usage() {
    cat >&2 <<'USAGE'
Usage: run.sh --project PROJECT_ROOT [--godot GODOT_BIN]
              [--api EXTENSION_API] [--session SESSION_JSON]

Required (flag or env):
  --project PATH   / PROJECT_ROOT=PATH    Godot project root directory.

One of these required unless --api is supplied:
  --godot   PATH   / GODOT_BIN=PATH       Godot 4.x binary.

Optional:
  --api     PATH   / EXTENSION_API=PATH   Pre-dumped extension_api.json.
  --session PATH                          Session template JSON.
                                          Default: scripts/m6-acceptance/walk.json

Example:
  run.sh --project ~/projects/pixelorama --godot /usr/local/bin/godot4
USAGE
    exit 1
}

die()  { echo "ERROR: $*" >&2; exit 1; }
info() { echo "[m6-acceptance] $*"; }

# ---------------------------------------------------------------------------
# Parse arguments
# ---------------------------------------------------------------------------

SESSION_JSON=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --project)  PROJECT_ROOT="${2:?'--project requires a path'}"; shift 2 ;;
        --godot)    GODOT_BIN="${2:?'--godot requires a path'}";      shift 2 ;;
        --api)      EXTENSION_API="${2:?'--api requires a path'}";    shift 2 ;;
        --session)  SESSION_JSON="${2:?'--session requires a path'}"; shift 2 ;;
        --help|-h)  usage ;;
        *)          die "Unknown argument: '$1' (use --help for usage)" ;;
    esac
done

# Validate required inputs
: "${PROJECT_ROOT:?'PROJECT_ROOT is not set. Pass --project or export the environment variable.'}"
[[ -d "${PROJECT_ROOT}" ]] \
    || die "PROJECT_ROOT '${PROJECT_ROOT}' is not a directory."
PROJECT_ROOT="$(cd "${PROJECT_ROOT}" && pwd)"   # canonicalize

[[ -z "${SESSION_JSON}" ]] && SESSION_JSON="${SCRIPT_DIR}/walk.json"
[[ -f "${SESSION_JSON}" ]] || die "Session template '${SESSION_JSON}' not found."

# ---------------------------------------------------------------------------
# Step 1: Obtain extension_api.json
# ---------------------------------------------------------------------------

if [[ -z "${EXTENSION_API:-}" ]]; then
    : "${GODOT_BIN:?'GODOT_BIN is not set and --api was not supplied. Pass --godot or export GODOT_BIN.'}"
    [[ -x "${GODOT_BIN}" ]] \
        || die "GODOT_BIN '${GODOT_BIN}' is not an executable file."

    info "Dumping extension_api.json via ${GODOT_BIN} ..."
    API_TMPDIR="$(mktemp -d)"
    trap 'rm -rf "${API_TMPDIR:-}"' EXIT
    # --dump-extension-api-with-docs writes extension_api.json to cwd; --headless suppresses GPU init.
    (
        cd "${API_TMPDIR}"
        "${GODOT_BIN}" --dump-extension-api-with-docs --headless 2>/dev/null || true
    )
    [[ -f "${API_TMPDIR}/extension_api.json" ]] \
        || die "Godot did not produce extension_api.json in '${API_TMPDIR}'. Verify GODOT_BIN is a Godot 4.x build."
    EXTENSION_API="${API_TMPDIR}/extension_api.json"
    info "extension_api.json dumped to ${EXTENSION_API}"
else
    [[ -f "${EXTENSION_API}" ]] || die "EXTENSION_API '${EXTENSION_API}' does not exist."
    info "Using extension_api.json: ${EXTENSION_API}"
fi
# Canonicalize
EXTENSION_API="$(cd "$(dirname "${EXTENSION_API}")" && pwd)/$(basename "${EXTENSION_API}")"

# ---------------------------------------------------------------------------
# Step 2: Build gdls
# ---------------------------------------------------------------------------

info "Building target/release/gdls ..."
(cd "${REPO_ROOT}" && cargo build --release --bin gdls)
GDLS="${REPO_ROOT}/target/release/gdls"
[[ -x "${GDLS}" ]] || die "Build produced no executable at '${GDLS}'."
info "gdls built: ${GDLS}"

# ---------------------------------------------------------------------------
# Step 3: Produce a concrete session from the template
#
# Substituted tokens:
#   __PROJECT_ROOT__    -> absolute project root
#   __EXTENSION_API__   -> absolute extension_api.json path
# File-placeholder tokens (__CALLER_FILE__, __CALLEE_FILE__, etc.) are not
# substituted here — they must already be filled in a project-specific session
# (see README.md for how to build one).
# ---------------------------------------------------------------------------

OUT_DIR="${REPO_ROOT}/target/m6-acceptance"
mkdir -p "${OUT_DIR}"

CONCRETE_SESSION="${OUT_DIR}/concrete-session.json"
info "Substituting template tokens ..."
sed \
    -e "s|__PROJECT_ROOT__|${PROJECT_ROOT}|g" \
    -e "s|__EXTENSION_API__|${EXTENSION_API}|g" \
    "${SESSION_JSON}" > "${CONCRETE_SESSION}"
info "Concrete session: ${CONCRETE_SESSION}"

# Guard: if the concrete session still contains unfilled file-role placeholders
# (e.g. __CALLER_FILE__, __CALLEE_FILE__) the capability walk will crash with a
# confusing FileNotFoundError.  Detect this early and give a clear message.
if grep -q '__[A-Z_]*_FILE__' "${CONCRETE_SESSION}" 2>/dev/null; then
    remaining="$(grep -o '__[A-Z_]*_FILE__' "${CONCRETE_SESSION}" | sort -u | tr '\n' ' ')"
    die "Session still contains unfilled file-role placeholders: ${remaining}
Copy walk.json into scripts/m6-acceptance/sessions/<project>.json, replace
each __*_FILE__ token with the relative path (from project root) to a real
project file, fill in real positions, then pass --session <your-session.json>.
See scripts/m6-acceptance/README.md for step-by-step instructions."
fi

# ---------------------------------------------------------------------------
# Step 4: Capability walk
# ---------------------------------------------------------------------------

CAPABILITY_REPORT="${OUT_DIR}/capability-report.json"
info "Running capability walk ..."
python3 "${REPO_ROOT}/scripts/lsp-poke.py" \
    --session "${CONCRETE_SESSION}" \
    --gdls "${GDLS}" \
    --out "${CAPABILITY_REPORT}"
info "Capability report: ${CAPABILITY_REPORT}"

# ---------------------------------------------------------------------------
# Step 5: Cold-vs-warm cache bench
#
# Strategy: lsp-poke.py writes elapsed_ms (wall time for the full session,
# including server startup) to its JSON output. We run a minimal session
# (initialize → shutdown, no opens, no requests) twice — cold then warm —
# and compare elapsed_ms values.
#
# Why initialize→shutdown is sufficient: server.rs runs workspace.reconcile()
# synchronously BEFORE entering the event loop (lines ~261-276 of server.rs).
# This means the cold walk (full .gd scan) blocks all replies, so even a bare
# initialize + shutdown captures the full startup-to-ready cost.  We keep the
# bench session dead-simple to avoid any file-open / URI confusion.
# ---------------------------------------------------------------------------

info "Building bench session ..."
BENCH_SESSION="${OUT_DIR}/bench-session.json"
COLD_REPORT="${OUT_DIR}/bench-cold.json"
WARM_REPORT="${OUT_DIR}/bench-warm.json"

python3 - "${PROJECT_ROOT}" "${EXTENSION_API}" "${BENCH_SESSION}" <<'PYEOF'
import json, sys

project_root, extension_api, bench_session = sys.argv[1:]

bench = {
    "initializationOptions": {
        "projectRoot": project_root,
        "extensionApiPath": extension_api,
    },
    "opens": [],
    "requests": [],
}
with open(bench_session, "w") as f:
    json.dump(bench, f, indent=2)
PYEOF

# Cold run — clear cache first.
info "Cold bench run (clearing .gdls/) ..."
rm -rf "${PROJECT_ROOT}/.gdls"
python3 "${REPO_ROOT}/scripts/lsp-poke.py" \
    --session "${BENCH_SESSION}" \
    --gdls "${GDLS}" \
    --out "${COLD_REPORT}"

# Warm run — cache written by cold run.
info "Warm bench run ..."
python3 "${REPO_ROOT}/scripts/lsp-poke.py" \
    --session "${BENCH_SESSION}" \
    --gdls "${GDLS}" \
    --out "${WARM_REPORT}"

# ---------------------------------------------------------------------------
# Step 6: Validate — write oss-report.json, exit non-zero on any failure
# ---------------------------------------------------------------------------

info "Validating results ..."

python3 - \
    "${CAPABILITY_REPORT}" \
    "${COLD_REPORT}" \
    "${WARM_REPORT}" \
    "${OUT_DIR}/oss-report.json" \
<<'PYEOF'
import json, sys, os

cap_report_path, cold_path, warm_path, report_path = sys.argv[1:]

REQUIRED_LABELS = {
    "hover/cross_file_method",
    "definition/class_name_expr",
    "definition/preload_string",
    "definition/autoload_name",
    "references/cross_file_method",
    "documentSymbol/nested_members",
    "implementation/func_with_overrides",
    "documentLink/res_literal",
}

SPEEDUP_THRESHOLD = 5.0
failures = []

# ---- Capability completeness ----
with open(cap_report_path) as f:
    cap = json.load(f)

requests = cap.get("requests", [])

for req in requests:
    label = req.get("label")
    if label not in REQUIRED_LABELS:
        continue
    resp = req.get("response")
    if resp is None:
        failures.append(f"CAPABILITY '{label}': no response (server returned nothing)")
        continue
    error = resp.get("error")
    if error is not None:
        failures.append(f"CAPABILITY '{label}': LSP error — {error}")
        continue
    result = resp.get("result")
    if result is None:
        failures.append(f"CAPABILITY '{label}': result is null")
        continue

    # Per-capability content assertions
    if label == "hover/cross_file_method":
        contents = result.get("contents") if isinstance(result, dict) else None
        value = ""
        if isinstance(contents, dict):
            value = contents.get("value", "")
        elif isinstance(contents, str):
            value = contents
        elif isinstance(contents, list) and contents:
            first = contents[0]
            value = first.get("value", first) if isinstance(first, dict) else str(first)
        if "func" not in value and "->" not in value:
            failures.append(
                f"CAPABILITY '{label}': hover does not look like a function signature "
                f"(want 'func'/'->'; got {repr(value[:200])})"
            )

    elif label in {
        "definition/class_name_expr",
        "definition/preload_string",
        "definition/autoload_name",
        "references/cross_file_method",
        "implementation/func_with_overrides",
    }:
        # result must be a non-empty list of locations
        if not isinstance(result, list) or len(result) == 0:
            failures.append(
                f"CAPABILITY '{label}': expected non-empty locations array, got {repr(result)[:200]}"
            )

    elif label == "documentSymbol/nested_members":
        if not isinstance(result, list) or len(result) == 0:
            failures.append(
                f"CAPABILITY '{label}': expected non-empty symbol array, got {repr(result)[:200]}"
            )

    elif label == "documentLink/res_literal":
        if not isinstance(result, list) or len(result) == 0:
            failures.append(
                f"CAPABILITY '{label}': expected non-empty document-link array, got {repr(result)[:200]}"
            )

# Check all required labels were present in the session
seen = {r.get("label") for r in requests}
missing = REQUIRED_LABELS - seen
if missing:
    failures.append(
        f"CAPABILITY: session is missing required labels: {sorted(missing)}. "
        "Fill in real positions for your project in the session template."
    )

# ---- Cold/warm speedup ----
with open(cold_path) as f:
    cold = json.load(f)
with open(warm_path) as f:
    warm = json.load(f)

cold_ms = cold.get("elapsed_ms", 0)
warm_ms = warm.get("elapsed_ms", 0)
ratio = None

if warm_ms <= 0 or cold_ms <= 0:
    failures.append(
        f"SPEEDUP: unusable timing values (cold={cold_ms}ms, warm={warm_ms}ms)"
    )
else:
    ratio = cold_ms / warm_ms
    tag = "OK" if ratio >= SPEEDUP_THRESHOLD else "FAIL"
    print(f"[m6-acceptance] cold={cold_ms}ms  warm={warm_ms}ms  ratio={ratio:.2f}x  [{tag}]")
    if ratio < SPEEDUP_THRESHOLD:
        failures.append(
            f"SPEEDUP: {ratio:.2f}x — below required {SPEEDUP_THRESHOLD}x "
            f"(cold={cold_ms}ms, warm={warm_ms}ms)"
        )

# ---- Write oss-report.json ----
report = {
    "capability_report": cap_report_path,
    "bench_cold_ms": cold_ms,
    "bench_warm_ms": warm_ms,
    "speedup_ratio": round(ratio, 2) if ratio is not None else None,
    "failures": failures,
    "pass": len(failures) == 0,
}
os.makedirs(os.path.dirname(report_path), exist_ok=True)
with open(report_path, "w") as f:
    json.dump(report, f, indent=2)
print(f"[m6-acceptance] Report: {report_path}")

if failures:
    print("\n[m6-acceptance] FAILED:", file=sys.stderr)
    for msg in failures:
        print(f"  - {msg}", file=sys.stderr)
    sys.exit(1)

print("[m6-acceptance] PASSED — all capabilities present and speedup >= 5x.")
PYEOF
