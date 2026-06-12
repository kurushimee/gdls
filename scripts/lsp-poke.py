#!/usr/bin/env python3
"""Drive a gdls LSP session over stdio: initialize, open files, fire a scripted list of
requests, and record every JSON-RPC reply plus the server's stderr trace.

The plan's Phase H (WP-Q2) calls for a helper that "opens a stdio LSP connection, sends
canned requests, and prints replies" so capabilities a GUI client can't trigger directly
(raw `callHierarchy/outgoingCalls`, `$/cancelRequest`, ...) can still be exercised by hand
against a real workspace.

The session is fully synchronous request/response: each request is sent with a unique id and
we read frames until that id's reply arrives, collecting any server->client notifications
(publishDiagnostics, ...) seen along the way. stderr is drained on a background thread so the
server's `GDLS_LOG=info` trace can't deadlock on a full pipe.

Framing follows the LSP base protocol: `Content-Length: <bytes>\\r\\n\\r\\n<utf8-json>`. The body
length is a BYTE count, so all reads/writes go through the raw binary streams.

Session file shape (JSON):
    {
      "initializationOptions": { "projectRoot": "...", "extensionApiPath": "..." },
      "opens":   [ "/path/to/a.gd", { "uri": "...", "text": "..." } ],
      "requests":[ { "label": "...", "method": "textDocument/hover", "params": {...} },
                   { "label": "...", "notification": true, "method": "$/cancelRequest",
                     "params": {"id": 7} },
                   { "label": "...", "action": "sleep", "ms": 400 },
                   { "label": "...", "action": "writefile", "path": "...", "text": "..." },
                   { "label": "...", "action": "deletefile", "path": "..." } ]
    }

Usage:
    scripts/lsp-poke.py --session SESSION.json [--gdls PATH] [--out OUT.json]
                        [--stderr STDERR.log] [--timeout SEC] [--log-filter FILTER]
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import threading
import time


def send_frame(stream, obj):
    body = json.dumps(obj, separators=(",", ":")).encode("utf-8")
    stream.write(f"Content-Length: {len(body)}\r\n\r\n".encode("ascii"))
    stream.write(body)
    stream.flush()


def read_frame(stream):
    """Read one LSP frame. Returns the parsed object, or None at EOF."""
    header = b""
    while not header.endswith(b"\r\n\r\n"):
        b = stream.read(1)
        if not b:
            return None  # EOF
        header += b
    length = 0
    for raw in header.split(b"\r\n"):
        line = raw.decode("ascii", "replace")
        if line.lower().startswith("content-length:"):
            length = int(line.split(":", 1)[1].strip())
    if length <= 0:
        return {}
    buf = b""
    while len(buf) < length:
        chunk = stream.read(length - len(buf))
        if not chunk:
            return None  # EOF mid-body
        buf += chunk
    return json.loads(buf.decode("utf-8"))


def path_to_uri(path):
    p = os.path.abspath(path).replace("\\", "/")
    if not p.startswith("/"):
        p = "/" + p
    return "file://" + p


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--gdls", default=os.path.join("target", "release", "gdls"),
                        help="path to the gdls binary (default: target/release/gdls)")
    parser.add_argument("--session", required=True, help="path to a JSON session script")
    parser.add_argument("--out", help="results JSON (default: alongside --session, *.results.json)")
    parser.add_argument("--stderr", dest="stderr_file",
                        help="captured server stderr (default: alongside --session, *.stderr.log)")
    parser.add_argument("--timeout", type=int, default=120,
                        help="watchdog: kill the server if the session exceeds this many seconds")
    parser.add_argument("--log-filter", default="info", help="value for GDLS_LOG (default: info)")
    args = parser.parse_args()

    base, _ = os.path.splitext(args.session)
    out_file = args.out or base + ".results.json"
    stderr_file = args.stderr_file or base + ".stderr.log"

    with open(args.session, encoding="utf-8") as fh:
        session = json.load(fh)

    init_options = session.get("initializationOptions", {})

    env = dict(os.environ)
    env["GDLS_LOG"] = args.log_filter
    proc = subprocess.Popen(
        [args.gdls],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env,
    )
    assert proc.stdin is not None and proc.stdout is not None and proc.stderr is not None
    stdin, stdout, stderr_pipe = proc.stdin, proc.stdout, proc.stderr

    # Drain stderr on a background thread so the server's trace can't fill the pipe and stall.
    stderr_lines = []

    def drain_stderr():
        for raw in stderr_pipe:
            stderr_lines.append(raw.decode("utf-8", "replace").rstrip("\n"))

    err_thread = threading.Thread(target=drain_stderr, daemon=True)
    err_thread.start()

    # Watchdog: kill on overrun so a hung request can't block the driver forever.
    def watchdog():
        if proc.poll() is None:
            proc.kill()

    timer = threading.Timer(args.timeout, watchdog)
    timer.start()

    results = []
    notifications = []
    next_id = 1
    start = time.monotonic()

    def pump_until(want_id, max_frames=2000):
        for _ in range(max_frames):
            msg = read_frame(stdout)
            if msg is None:
                return None
            if msg.get("id") is not None and "method" not in msg:
                if msg["id"] == want_id:
                    return msg
                results.append({"label": f"<orphan id={msg['id']}>", "response": msg})
            else:
                notifications.append(msg)
        return None

    try:
        # 1) initialize handshake
        init_id = next_id
        next_id += 1
        root = init_options.get("projectRoot", "")
        init_params = {
            "processId": os.getpid(),
            "rootUri": path_to_uri(root) if root else None,
            # M7 §7.4: a vendored editor profile (tests/fixtures/client_caps/*.json) can be
            # replayed against a real binary via the session file's "capabilities" key; the
            # bare default keeps the historical capability-less walk.
            "capabilities": session.get("capabilities", {"textDocument": {}, "workspace": {}}),
            "initializationOptions": init_options,
        }
        send_frame(stdin, {"jsonrpc": "2.0", "id": init_id, "method": "initialize", "params": init_params})
        init_resp = pump_until(init_id)
        results.append({"label": "initialize", "method": "initialize", "id": init_id, "response": init_resp})

        # 2) initialized notification — triggers cold-index reconcile
        send_frame(stdin, {"jsonrpc": "2.0", "method": "initialized", "params": {}})

        # 3) open files the walk will query (didOpen builds the server-side rope). An entry may be
        #    a path string (content read from disk) or an object { uri, text } for an inline buffer.
        for f in session.get("opens", []):
            if isinstance(f, str):
                uri = path_to_uri(f)
                with open(f, encoding="utf-8") as src:
                    text = src.read()
            elif "text" in f:
                uri = f["uri"]
                text = f["text"]
            else:
                uri = path_to_uri(f["path"])
                with open(f["path"], encoding="utf-8") as src:
                    text = src.read()
            send_frame(stdin, {
                "jsonrpc": "2.0", "method": "textDocument/didOpen",
                "params": {"textDocument": {"uri": uri, "languageId": "gdscript", "version": 1, "text": text}},
            })

        # 4) scripted requests
        for r in session.get("requests", []):
            # Side-effect actions for the timing-sensitive rows (diagnostics / watcher freshness):
            # mutate the filesystem or pause mid-session, then let the drain capture the server's
            # pushed reaction (e.g. a watcher-driven index update or a re-published diagnostic).
            if "action" in r:
                action = r["action"]
                if action == "sleep":
                    time.sleep(int(r["ms"]) / 1000.0)
                elif action == "writefile":
                    with open(r["path"], "w", encoding="utf-8") as wf:
                        wf.write(r["text"])
                elif action == "deletefile":
                    try:
                        os.remove(r["path"])
                    except OSError:
                        pass
                results.append({"label": r.get("label"), "action": action})
                continue
            if r.get("notification"):
                send_frame(stdin, {"jsonrpc": "2.0", "method": r["method"], "params": r.get("params")})
                results.append({"label": r.get("label"), "method": r["method"], "notification": True})
                continue
            rid = next_id
            next_id += 1
            send_frame(stdin, {"jsonrpc": "2.0", "id": rid, "method": r["method"], "params": r.get("params")})
            resp = pump_until(rid)
            results.append({"label": r.get("label"), "method": r["method"], "id": rid, "response": resp})

        # 5) shutdown + exit
        shut_id = next_id
        next_id += 1
        send_frame(stdin, {"jsonrpc": "2.0", "id": shut_id, "method": "shutdown", "params": None})
        shut_resp = pump_until(shut_id)
        results.append({"label": "shutdown", "method": "shutdown", "id": shut_id, "response": shut_resp})
        send_frame(stdin, {"jsonrpc": "2.0", "method": "exit", "params": None})
        stdin.close()

        # 6) drain remaining notifications to EOF
        while True:
            msg = read_frame(stdout)
            if msg is None:
                break
            if msg.get("id") is not None and "method" not in msg:
                results.append({"label": f"<tail id={msg['id']}>", "response": msg})
            else:
                notifications.append(msg)

        try:
            proc.wait(timeout=args.timeout)
        except subprocess.TimeoutExpired:
            proc.kill()
    finally:
        timer.cancel()
        time.sleep(0.15)  # let the async stderr reader flush
        err_thread.join(timeout=1.0)

    elapsed_ms = int((time.monotonic() - start) * 1000)
    out = {
        "gdls": args.gdls,
        "session": args.session,
        "elapsed_ms": elapsed_ms,
        "exit_code": proc.returncode,
        "requests": results,
        "notifications": notifications,
    }
    with open(out_file, "w", encoding="utf-8") as wf:
        json.dump(out, wf, indent=2)
    with open(stderr_file, "w", encoding="utf-8") as wf:
        wf.write("\n".join(stderr_lines))

    print(f"results -> {out_file}")
    print(f"stderr  -> {stderr_file}  ({len(stderr_lines)} lines)")
    print(f"elapsed -> {elapsed_ms} ms, exit={proc.returncode}")


if __name__ == "__main__":
    main()
