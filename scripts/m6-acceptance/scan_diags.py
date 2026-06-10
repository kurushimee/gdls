#!/usr/bin/env python3
"""Full-project diagnostics sweep — the release gate a nav-row walk cannot be.

Opens EVERY `.gd` under --project against a gdls binary (didOpen, full text), collects every
`textDocument/publishDiagnostics`, and writes a JSON report with severity + error-message
histograms and per-file error lists.

Why this exists: the v1.0.0 acceptance walks were nav-row based (hover/definition/references at
curated positions) and every opened file happened to extend a native class directly — so a
diagnostics false-positive epidemic at project scale went unseen (133/243 Pixelorama files
carried bogus errors). Run this on the acceptance projects before ANY release;
`files_with_errors` must be ~0, and every remainder individually justified against
`godot --check-only`.

Usage:
  scan_diags.py --project <root> --gdls <path/to/gdls> [--api <extension_api.json>] [--out report.json]

With no --api the server's own auto-dump resolution applies (pass --api to pin a dump and keep
the gate hermetic).
"""

import argparse
import json
import os
import subprocess
import sys
import threading
import time

SKIP_DIRS = {".godot", ".git", ".gdls", ".import"}


def frame(obj):
    body = json.dumps(obj).encode("utf-8")
    return b"Content-Length: " + str(len(body)).encode() + b"\r\n\r\n" + body


def uri_of(path):
    path = os.path.abspath(path).replace("\\", "/")
    if not path.startswith("/"):
        path = "/" + path  # windows drive form: file:///C:/...
    return "file://" + path


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--project", required=True, help="project root (contains project.godot)")
    ap.add_argument("--gdls", required=True, help="path to the gdls binary")
    ap.add_argument("--api", help="extension_api.json to pin (optional)")
    ap.add_argument("--out", default="scan-report.json", help="report output path")
    ap.add_argument("--timeout", type=int, default=600, help="shutdown barrier timeout (s)")
    args = ap.parse_args()

    root = os.path.abspath(args.project)
    files = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for f in filenames:
            if f.endswith(".gd"):
                files.append(os.path.join(dirpath, f).replace("\\", "/"))
    files.sort()
    print(f"scanning {len(files)} .gd files under {root}", flush=True)

    stderr_log = open(os.path.splitext(args.out)[0] + "-stderr.log", "wb")
    proc = subprocess.Popen(
        [args.gdls], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr_log
    )
    assert proc.stdin is not None and proc.stdout is not None  # PIPEs above guarantee both
    stdin, stdout = proc.stdin, proc.stdout

    diags = {}  # uri (lowercased) -> latest publishDiagnostics params
    responses = {}  # id -> message
    lock = threading.Lock()

    def reader():
        buf = stdout
        while True:
            line = buf.readline()
            if not line:
                return
            if not line.lower().startswith(b"content-length:"):
                continue
            n = int(line.split(b":")[1].strip())
            while True:
                line = buf.readline()
                if line in (b"\r\n", b"\n", b""):
                    break
            body = buf.read(n)
            try:
                msg = json.loads(body)
            except Exception:
                continue
            with lock:
                if msg.get("method") == "textDocument/publishDiagnostics":
                    diags[msg["params"]["uri"].lower()] = msg["params"]
                elif "id" in msg and ("result" in msg or "error" in msg):
                    responses[msg["id"]] = msg

    threading.Thread(target=reader, daemon=True).start()

    def send(obj):
        stdin.write(frame(obj))
        stdin.flush()

    init_options = {"projectRoot": root}
    if args.api:
        init_options["extensionApiPath"] = os.path.abspath(args.api)
        init_options["autoDumpExtensionApi"] = False
    send(
        {
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "processId": os.getpid(),
                "rootUri": uri_of(root),
                "capabilities": {},
                "initializationOptions": init_options,
            },
        }
    )
    deadline = time.time() + 180
    while 1 not in responses and time.time() < deadline:
        time.sleep(0.05)
    assert 1 in responses, "initialize timed out"
    send({"jsonrpc": "2.0", "method": "initialized", "params": {}})

    start = time.time()
    unreadable = []
    for i, path in enumerate(files):
        try:
            with open(path, encoding="utf-8") as fh:
                text = fh.read()
        except (UnicodeDecodeError, OSError) as e:
            unreadable.append((path, str(e)))
            continue
        send(
            {
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {
                    "textDocument": {
                        "uri": uri_of(path),
                        "languageId": "gdscript",
                        "version": 1,
                        "text": text,
                    }
                },
            }
        )
        if (i + 1) % 250 == 0:
            print(f"  opened {i + 1}/{len(files)}", flush=True)

    # FIFO barrier: the shutdown response means every didOpen before it was processed and its
    # publishDiagnostics already written to stdout ahead of the response.
    send({"jsonrpc": "2.0", "id": 9999, "method": "shutdown", "params": None})
    deadline = time.time() + args.timeout
    while 9999 not in responses and time.time() < deadline:
        time.sleep(0.2)
    barrier_ok = 9999 in responses
    send({"jsonrpc": "2.0", "method": "exit", "params": None})
    time.sleep(0.5)
    stdin.close()

    with lock:
        snapshot = dict(diags)

    err_files, warn_only_files = [], []
    sev_hist = {}
    msg_hist = {}
    for uri, p in snapshot.items():
        errs = [d for d in p.get("diagnostics", []) if d.get("severity") == 1]
        warns = [d for d in p.get("diagnostics", []) if d.get("severity") == 2]
        for d in p.get("diagnostics", []):
            sev_hist[str(d.get("severity"))] = sev_hist.get(str(d.get("severity")), 0) + 1
        if errs:
            err_files.append(
                {
                    "uri": uri,
                    "error_count": len(errs),
                    "first": {
                        "line": errs[0]["range"]["start"]["line"],
                        "message": errs[0]["message"],
                    },
                    "messages": sorted({e["message"] for e in errs})[:6],
                }
            )
            for e in errs:
                key = e["message"][:90]
                msg_hist[key] = msg_hist.get(key, 0) + 1
        elif warns:
            warn_only_files.append(uri)

    err_files.sort(key=lambda x: -x["error_count"])
    report = {
        "barrier_ok": barrier_ok,
        "elapsed_s": round(time.time() - start, 1),
        "files_opened": len(files) - len(unreadable),
        "unreadable": unreadable,
        "files_with_diagnostics": len(snapshot),
        "severity_histogram": sev_hist,
        "files_with_errors": len(err_files),
        "files_warnings_only": len(warn_only_files),
        "error_message_histogram": dict(sorted(msg_hist.items(), key=lambda kv: -kv[1])),
        "error_files": err_files,
    }
    with open(args.out, "w") as fh:
        json.dump(report, fh, indent=2)
    total_errs = sum(e["error_count"] for e in err_files)
    print(
        f"barrier_ok={barrier_ok} elapsed={report['elapsed_s']}s "
        f"files_with_errors={len(err_files)} total_errors={total_errs} sev={sev_hist}",
        flush=True,
    )
    print(f"report -> {args.out}", flush=True)
    return 0 if barrier_ok else 1


if __name__ == "__main__":
    sys.exit(main())
