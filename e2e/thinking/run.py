"""Windows live acceptance: Read, restart, and replay empty signed thinking.

The recording proxy caps output and selects an explicit thinking profile.
Credentials stay in memory; request bodies and raw SSE are local artifacts.
"""

import argparse
import json
import os
import re
import secrets
import select
import shutil
import tempfile
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit

import pyte
import requests
from winpty import PtyProcess

parser = argparse.ArgumentParser()
parser.add_argument("binary", type=Path)
parser.add_argument("--profile", default="sudorouter")
parser.add_argument("--model", default="claude-sonnet-4-6")
parser.add_argument(
    "--config", type=Path, default=Path.home() / ".nexus/sudocode/sudocode.json"
)
parser.add_argument(
    "--artifacts",
    type=Path,
    help="Keep request bodies, SSE and screens in a fresh child directory",
)
args = parser.parse_args()
if os.name != "nt":
    parser.error("this acceptance runner requires Windows ConPTY")
if not args.binary.is_file():
    parser.error("binary must be an existing scode executable")
code = "LIVE-" + secrets.token_hex(5)
fixture_root = Path(os.environ["PUBLIC"]).resolve()
temporary = tempfile.TemporaryDirectory(prefix="scode-" + code + "-", dir=fixture_root)
workspace = Path(temporary.name).resolve()
assert workspace.parent == fixture_root
for ancestor in (workspace, *workspace.parents):
    for name in (
        "AGENTS.md",
        ".nexus/sudocode/AGENTS.md",
        "CLAUDE.md",
        ".claude/CLAUDE.md",
    ):
        assert not (ancestor / name).exists(), "fixture inherits external instructions"
config = workspace / "config"
home = workspace / "home"
config.mkdir()
home.mkdir()
fixture = workspace / "values.txt"
a, b = secrets.randbelow(400) + 100, secrets.randbelow(400) + 100
fixture.write_text(f"code={code}\na={a}\nb={b}\n", encoding="utf-8")
session = workspace / "session.jsonl"
stamp = int(time.time() * 1000)
session.write_text(
    json.dumps(
        {
            "type": "session_meta",
            "version": 1,
            "session_id": str(uuid.uuid4()),
            "created_at_ms": stamp,
            "updated_at_ms": stamp,
            "model": args.model,
            "workspace_root": str(workspace),
        }
    )
    + "\n",
    encoding="utf-8",
)
profile = json.loads(args.config.read_text(encoding="utf-8-sig"))["auth_modes"][
    "proxy"
][args.profile]
target = urlsplit(profile["baseUrl"])
assert target.scheme == "https"
origin = target.scheme + "://" + target.netloc
records = []
errors = []
lock = threading.Lock()


def parse_sse(payload):
    blocks = {}
    usage = {}
    stopped = False
    for line in payload.decode("utf-8", errors="replace").splitlines():
        if not line.startswith("data:"):
            continue
        try:
            event = json.loads(line[5:])
        except ValueError:
            continue
        if event.get("type") == "content_block_start":
            blocks[event["index"]] = dict(event["content_block"])
        if event.get("type") == "content_block_delta":
            block = blocks[event["index"]]
            for key in ("text", "thinking", "signature", "partial_json"):
                if key in event["delta"]:
                    block[key] = block.get(key, "") + event["delta"][key]
        if event.get("type") == "message_start":
            usage.update(event.get("message", {}).get("usage", {}))
        usage.update(event.get("usage", {}))
        stopped |= event.get("type") == "message_stop"
    return list(blocks.values()), usage, stopped


class Proxy(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def forward(self, method):
        raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
        is_message = method == "POST" and self.path.split("?")[0].endswith("/messages")
        if is_message:
            with lock:
                number = len(records)
                assert number < 5, "request budget exhausted"
                records.append({"number": number, "completed": False})
            (workspace / f"request-{number}-client.json").write_bytes(raw)
            body = json.loads(raw)
            body["max_tokens"] = 2048
            # Omitted summaries cause the real provider to emit an empty signed
            # block. Hold the same bounded profile throughout the tool journey.
            body["thinking"] = {"type": "adaptive", "display": "omitted"}
            body["output_config"] = {"effort": "low"}
            raw = json.dumps(body, ensure_ascii=False).encode()
            (workspace / f"request-{number}-live.json").write_bytes(raw)
        headers = {
            k: v
            for k, v in self.headers.items()
            if k.lower()
            not in {
                "host",
                "content-length",
                "authorization",
                "x-api-key",
                "connection",
                "accept-encoding",
            }
        }
        headers.update({"x-api-key": profile["apiKey"], "accept-encoding": "identity"})
        try:
            with (
                requests.Session() as client,
                client.request(
                    method,
                    origin + self.path,
                    headers=headers,
                    data=raw or None,
                    stream=True,
                    timeout=(20, 100),
                ) as response,
            ):
                self.send_response(response.status_code)
                self.send_header(
                    "Content-Type",
                    response.headers.get("content-type", "application/json"),
                )
                self.send_header("Connection", "close")
                self.end_headers()
                chunks = []
                for chunk in response.iter_content(chunk_size=512):
                    if not chunk:
                        continue
                    chunks.append(chunk)
                    try:
                        self.wfile.write(chunk)
                        self.wfile.flush()
                    except (
                        BrokenPipeError,
                        ConnectionResetError,
                        ConnectionAbortedError,
                    ):
                        pass
                if is_message:
                    payload = b"".join(chunks)
                    (workspace / f"response-{number}.sse").write_bytes(payload)
                    blocks, usage, stopped = parse_sse(payload)
                    text = "\n".join(b.get("text", "") for b in blocks)
                    records[number].update(
                        {
                            "completed": True,
                            "http": response.status_code,
                            "request_id": response.headers.get("x-oneapi-request-id"),
                            "blocks": [b["type"] for b in blocks],
                            "thinking": [
                                {
                                    "text_chars": len(b.get("thinking", "")),
                                    "signature_chars": len(b.get("signature", "")),
                                }
                                for b in blocks
                                if b["type"] == "thinking"
                            ],
                            "message_stop": stopped,
                            "usage": usage,
                            "artifact_words": re.findall(
                                r"(?im)^\s*(course|court|card)\s*$", text
                            ),
                        }
                    )
                    print(
                        json.dumps({"wire": records[number]}, ensure_ascii=False),
                        flush=True,
                    )
                    if response.status_code != 200:
                        errors.append("API HTTP " + str(response.status_code))
        except requests.RequestException as error:
            errors.append(type(error).__name__)
            if is_message:
                records[number]["transport_error"] = type(error).__name__

    def do_GET(self):
        self.forward("GET")

    def do_POST(self):
        self.forward("POST")


server = ThreadingHTTPServer(("127.0.0.1", 0), Proxy)
server.daemon_threads = True
threading.Thread(target=server.serve_forever, daemon=True).start()
(config / "sudocode.json").write_text(
    json.dumps(
        {
            "default_model": args.model,
            "auth_modes": {
                "proxy": {
                    "audit": {
                        "baseUrl": f"http://127.0.0.1:{server.server_port}/v1",
                        "apiKey": "local-placeholder",
                    }
                }
            },
        }
    ),
    encoding="utf-8",
)
(config / "settings.json").write_text(
    json.dumps({"model": args.model, "auth_profile": "audit"}), encoding="utf-8"
)
env = {
    k: v
    for k, v in os.environ.items()
    if not k.upper().startswith(
        (
            "ANTHROPIC",
            "OPENAI",
            "SUDO_CODE",
            "SUDOCODE",
            "SCODE_",
            "CLAUDE",
            "VSCODE",
            "TERM_PROGRAM",
        )
    )
}
env.update(
    SUDO_CODE_CONFIG_HOME=str(config),
    HOME=str(home),
    USERPROFILE=str(home),
    NO_COLOR="1",
    SCODE_LOG_PATH=str(workspace / "cli.log"),
)
command = [
    str(args.binary.resolve()),
    "--auth",
    "proxy",
    "--account",
    "audit",
    "--model",
    args.model,
    "--permission-mode",
    "read-only",
    "--allowedTools",
    "Read",
    "--resume",
    str(session),
]
proc = None
raw_pty = []


def drain():
    if select.select([proc.fileobj], [], [], 0)[0]:
        try:
            chunk = proc.read(65536)
        except EOFError:
            chunk = ""
        if chunk:
            raw_pty.append(chunk)
            stream.feed(chunk)
            (workspace / "current-screen.txt").write_text(
                "\n".join(screen.display), encoding="utf-8"
            )


def visible():
    return (
        "\n".join(
            "".join(c.data for _, c in sorted(row.items()))
            for row in screen.history.top
        )
        + "\n"
        + "\n".join(screen.display)
    )


def wait(predicate, seconds=150):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        drain()
        assert not errors, errors
        if predicate():
            return
        assert proc.isalive(), "CLI exited unexpectedly"
        time.sleep(0.05)
    raise AssertionError("live ConPTY expected state timed out")


def start():
    global proc, screen, stream
    screen = pyte.HistoryScreen(140, 40, history=5000)
    stream = pyte.Stream(screen)
    proc = PtyProcess.spawn(command, cwd=str(workspace), env=env, dimensions=(40, 140))
    wait(lambda: "\u276f" in "\n".join(screen.display), 45)
    time.sleep(0.3)


def send(text):
    proc.write(text)
    time.sleep(0.15)
    proc.write("\r")


def close():
    if proc and proc.isalive():
        send("/exit")
        deadline = time.monotonic() + 8
        while proc.isalive() and time.monotonic() < deadline:
            drain()
            time.sleep(0.05)
        if proc.isalive():
            proc.terminate(force=True)


try:
    print(
        json.dumps(
            {
                "started": code,
                "workspace": str(workspace),
                "binary": str(args.binary),
                "profile": args.profile,
                "model": args.model,
                "thinking_control": "adaptive-omitted",
                "live_api_inference": True,
            }
        ),
        flush=True,
    )
    start()
    send(
        "Read values.txt with the Read tool. Return only READ_DONE <code from file> sum=<a+b>."
    )
    wait(lambda: f"READ_DONE {code} sum={a + b}" in visible())
    (workspace / "read-screen.txt").write_text(visible(), encoding="utf-8")
    close()
    fixture.rename(workspace / "values.unavailable")
    start()
    send(
        "Use the saved sum, subtract 17, and return only RESUME_DONE <same code> value=<result>. Do not use tools."
    )
    wait(lambda: f"RESUME_DONE {code} value={a + b - 17}" in visible())
    (workspace / "resume-screen.txt").write_text(visible(), encoding="utf-8")
    close()
    assert len(records) == 3, "unexpected tool or retry requests"
    first_blocks, _, _ = parse_sse((workspace / "response-0.sse").read_bytes())
    empty_signed = [
        b
        for b in first_blocks
        if b["type"] == "thinking" and not b.get("thinking") and b.get("signature")
    ]
    assert empty_signed, "provider did not emit the target empty signed thinking block"
    second = json.loads((workspace / "request-1-client.json").read_bytes())
    results = [
        b
        for m in second["messages"]
        for b in m.get("content", [])
        if isinstance(b, dict) and b.get("type") == "tool_result"
    ]
    assert results and not results[-1].get("is_error"), "Read must actually succeed"
    read_output = json.dumps(results[-1]["content"])
    assert all(value in read_output for value in (code, f"a={a}", f"b={b}")), (
        "actual Read output must contain fresh data"
    )
    replayed = [
        b
        for m in second["messages"]
        if m["role"] == "assistant"
        for b in m.get("content", [])
        if isinstance(b, dict)
    ]
    intact = all(
        any(
            b.get("type") == "thinking"
            and b.get("thinking") == ""
            and b.get("signature") == thought["signature"]
            for b in replayed
        )
        for thought in empty_signed
    )
    assert intact, (
        "empty signed thinking was dropped or changed before the tool continuation"
    )
    resumed = json.loads((workspace / "request-2-client.json").read_bytes())
    resumed_blocks = [
        b
        for m in resumed["messages"]
        if m["role"] == "assistant"
        for b in m.get("content", [])
        if isinstance(b, dict)
    ]
    assert all(
        any(
            b.get("type") == "thinking"
            and b.get("thinking") == ""
            and b.get("signature") == thought["signature"]
            for b in resumed_blocks
        )
        for thought in empty_signed
    ), "empty signed thinking was lost during restart/resume"
    assert all(
        r.get("completed") and r.get("http") == 200 and r.get("message_stop")
        for r in records
    ), "all streams must finish"
    for number, expected in (
        (1, f"READ_DONE {code} sum={a + b}"),
        (2, f"RESUME_DONE {code} value={a + b - 17}"),
    ):
        blocks, _, _ = parse_sse((workspace / f"response-{number}.sse").read_bytes())
        answer = "".join(block.get("text", "") for block in blocks).strip()
        assert answer == expected, "final answer must contain only the requested result"
    assert not any(r.get("artifact_words") for r in records), (
        "new course/court/card text generated"
    )
    print(
        json.dumps(
            {
                "passed_workflow": True,
                "empty_signed_thinking_replayed": intact,
                "requests": len(records),
                "live_api_inference": True,
            }
        ),
        flush=True,
    )
finally:
    try:
        close()
    finally:
        server.shutdown()
        server.server_close()
        (workspace / "pty-output.txt").write_text("".join(raw_pty), encoding="utf-8")
        try:
            if args.artifacts:
                destination = args.artifacts.resolve() / code
                destination.mkdir(parents=True, exist_ok=False)
                for path in workspace.iterdir():
                    if path.is_file():
                        shutil.copy2(path, destination / path.name)
                print(json.dumps({"artifacts": str(destination)}), flush=True)
        finally:
            assert workspace.resolve().parent == fixture_root
            temporary.cleanup()
