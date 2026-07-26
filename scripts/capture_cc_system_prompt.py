#!/usr/bin/env python3
"""Capture Claude Code's per-model system prompt (Tier-1 runtime observation).

Why this exists
---------------
`claude-trace` does NOT instrument the native Claude Code 2.x binary — it logs
zero request pairs. The working runtime-observation method is a tiny logging
HTTP proxy: point `ANTHROPIC_BASE_URL` at it, run `claude --model <id> -p hi`,
and CC assembles + sends its system prompt for that model *client-side* before
any real auth/availability check. The proxy captures the first request body and
returns a 400 so CC exits immediately.

CC serves DIFFERENT system-prompt variants per model. This tool tabulates which
variant each model gets (size + section headers). Run it after upgrading the
local CC binary to keep the roadmap's "System prompt — CC alignment" section
(Table A) honest.

IP note
-------
This prints only *derived facts* (char counts + `# Section` headers). It does
not commit CC's verbatim prompt text. If you pass --dump, raw captures are
written under a local, git-ignored directory — treat them like the private
CC source-leak snapshot: keep them local, do not commit.

Usage
-----
    python scripts/capture_cc_system_prompt.py
    python scripts/capture_cc_system_prompt.py --models claude-opus-5 claude-sonnet-5
    python scripts/capture_cc_system_prompt.py --dump ./_cc_capture   # local only
"""
from __future__ import annotations

import argparse
import http.server
import json
import os
import re
import subprocess
import threading
import time

DEFAULT_MODELS = [
    "claude-opus-4-8",
    "claude-opus-5",
    "claude-fable-5",
    "claude-sonnet-5",
    "claude-sonnet-4-5",
    "claude-opus-4-5",
    "claude-haiku-4-5",
    "claude-3-5-haiku-20241022",
]

PORT = 8788


def _make_server(state):
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            state["body"] = self.rfile.read(int(self.headers.get("Content-Length", 0)))
            self.send_response(400)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(b'{"type":"error","error":{"type":"invalid_request_error","message":"cap"}}')

        def do_GET(self):
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b"{}")

        def log_message(self, *_):  # silence
            pass

    return http.server.HTTPServer(("127.0.0.1", PORT), Handler)


def _system_text(body: bytes) -> tuple[str, str | None]:
    payload = json.loads(body)
    system = payload.get("system")
    if isinstance(system, list):
        text = "\n\n".join(b.get("text", "") for b in system if isinstance(b, dict))
    else:
        text = system if isinstance(system, str) else ""
    return text, payload.get("model")


def _sections(text: str) -> list[tuple[str, int]]:
    parts = re.split(r"(?m)^(#\s+.+)$", text)
    out = []
    for i in range(1, len(parts), 2):
        body = parts[i + 1] if i + 1 < len(parts) else ""
        out.append((parts[i].strip(), len(body)))
    return out


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--models", nargs="+", default=DEFAULT_MODELS, help="model ids to capture")
    ap.add_argument("--dump", metavar="DIR", help="local dir for raw captures (do NOT commit)")
    ap.add_argument("--timeout", type=int, default=40, help="per-model claude timeout (s)")
    args = ap.parse_args()

    state: dict[str, bytes | None] = {"body": None}
    server = _make_server(state)
    threading.Thread(target=server.serve_forever, daemon=True).start()

    env = {k: v for k, v in os.environ.items() if not k.startswith("CLAUDE_CODE_")}
    env["ANTHROPIC_BASE_URL"] = f"http://127.0.0.1:{PORT}"
    env.setdefault("ANTHROPIC_API_KEY", "sk-capture-dummy")

    if args.dump:
        os.makedirs(args.dump, exist_ok=True)

    print(f"{'requested model':<30}{'body.model':<28}{'chars':>7}  sections")
    print("-" * 110)
    for model in args.models:
        state["body"] = None
        try:
            subprocess.run(["claude", "--model", model, "-p", "hi"], env=env,
                           capture_output=True, timeout=args.timeout)
        except Exception:
            pass
        time.sleep(0.4)
        if state["body"] is None:
            print(f"{model:<30}{'--':<28}{0:>7}  NO-CAPTURE")
            continue
        text, req_model = _system_text(state["body"])
        heads = " | ".join(h for h, _ in _sections(text))
        print(f"{model:<30}{str(req_model):<28}{len(text):>7}  {heads[:60]}")
        if args.dump:
            path = os.path.join(args.dump, f"model_{model}.txt")
            with open(path, "w", encoding="utf-8") as fh:
                fh.write(text)

    server.shutdown()


if __name__ == "__main__":
    main()
