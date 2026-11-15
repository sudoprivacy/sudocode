#!/usr/bin/env python3
"""Exercise the real setup wizard and a release CLI with a local provider.

Unix runs the wizard through a PTY; Windows can run the same process journey
with Git Bash pipes. Neither establishes compatibility with a live provider.
"""
import argparse
import json
import os
from pathlib import Path
import select
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


ROOT = Path(__file__).resolve().parents[2]
KEY = "synthetic-setup-acceptance-key"
MODELS = ["claude-sonnet-4-6", "gpt-5.4"]


class Provider(BaseHTTPRequestHandler):
    requests = []

    def log_message(self, *_args):
        pass

    def do_GET(self):
        if self.headers.get("Authorization") != "Bearer " + KEY:
            self.send_error(403)
            return
        body = json.dumps({"data": [{"id": m} for m in MODELS]}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.requests.append((self.path, body["model"]))
        if self.path == "/v1/messages":
            events = [
                ("message_start", {"type": "message_start", "message": {
                    "id": "msg_setup", "type": "message", "role": "assistant",
                    "model": body["model"], "content": [], "stop_reason": None,
                    "usage": {"input_tokens": 8, "output_tokens": 0}}}),
                ("content_block_start", {"type": "content_block_start", "index": 0,
                    "content_block": {"type": "text", "text": ""}}),
                ("content_block_delta", {"type": "content_block_delta", "index": 0,
                    "delta": {"type": "text_delta", "text": "SETUP_OK"}}),
                ("content_block_stop", {"type": "content_block_stop", "index": 0}),
                ("message_delta", {"type": "message_delta",
                    "delta": {"stop_reason": "end_turn", "stop_sequence": None},
                    "usage": {"output_tokens": 3}}),
                ("message_stop", {"type": "message_stop"}),
            ]
        elif self.path == "/v1/chat/completions":
            events = [("message", {"id": "chat_setup", "object": "chat.completion.chunk",
                "model": body["model"], "choices": [{"index": 0,
                    "delta": {"role": "assistant", "content": "SETUP_OK"}, "finish_reason": None}]}),
                ("message", {"id": "chat_setup", "object": "chat.completion.chunk",
                    "model": body["model"], "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    "usage": {"prompt_tokens": 8, "completion_tokens": 3, "total_tokens": 11}})]
        else:
            self.send_error(400, "Unexpected protocol")
            return
        wire = "".join(f"event: {name}\ndata: {json.dumps(data)}\n\n" for name, data in events).encode()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(wire)))
        self.end_headers()
        self.wfile.write(wire)


def wizard(bash, config, endpoint, replies, env, expected=0):
    command = [bash, (ROOT / "packaging/macos/scode-setup.sh").as_posix(),
               "--config-dir", config.as_posix(), "--base-url", endpoint]
    if os.name == "nt":
        result = subprocess.run(command, input="".join(answer + "\n" for _, answer in replies).encode(),
                                env=env, capture_output=True, timeout=60)
        output, returncode = (result.stdout + result.stderr).decode("utf-8"), result.returncode
    else:
        import pty
        import termios
        master, slave = pty.openpty()
        process = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=slave, env=env)
        os.close(slave)
        output = ""
        offset = 0
        try:
            for prompt, answer in replies:
                deadline = time.monotonic() + 60
                while prompt not in output[offset:]:
                    if time.monotonic() > deadline:
                        raise AssertionError(f"Wizard did not reach {prompt!r}: {output}")
                    if select.select([master], [], [], 0.2)[0]:
                        try:
                            chunk = os.read(master, 65536)
                        except OSError:
                            chunk = b""
                        if not chunk:
                            raise AssertionError(f"Wizard exited before {prompt!r}: {output}")
                        output += chunk.decode("utf-8", errors="replace")
                offset = len(output)
                if prompt == "API Key (sk-...): ":
                    while termios.tcgetattr(master)[3] & termios.ECHO:
                        if time.monotonic() > deadline:
                            raise AssertionError("API key prompt did not disable echo")
                        time.sleep(0.01)
                os.write(master, (answer + "\n").encode())
            deadline = time.monotonic() + 60
            while process.poll() is None:
                if time.monotonic() > deadline:
                    raise AssertionError("Wizard did not finish")
                if select.select([master], [], [], 0.2)[0]:
                    try:
                        output += os.read(master, 65536).decode("utf-8", errors="replace")
                    except OSError:
                        break
            returncode = process.wait(timeout=5)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            os.close(master)
    assert returncode == expected, (returncode, output)
    assert KEY not in output, "Setup exposed the API key"
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scode", type=Path, required=True)
    parser.add_argument("--bash", default="/bin/bash")
    args = parser.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", 0), Provider)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    endpoint = f"http://127.0.0.1:{server.server_port}/v1"
    try:
        with tempfile.TemporaryDirectory(prefix="scode-setup-e2e-") as directory:
            root = Path(directory)
            workspace = root / "workspace"
            workspace.mkdir()
            env = os.environ.copy()
            for name in list(env):
                if name.startswith(("PROXY_", "ANTHROPIC_", "OPENAI_", "CLAUDE_CODE_", "NEXUS_")):
                    env.pop(name)
            env.update(HOME=str(root), USERPROFILE=str(root), SCODE_SERVICE_ROOT=str(root / "services"),
                       MSYS_NO_PATHCONV="1", NO_COLOR="1", SUDOCODE_API_READ_TIMEOUT="10")
            for i, model in enumerate(MODELS, 1):
                config = root / f"config-{i}"
                env["SUDO_CODE_CONFIG_HOME"] = str(config)
                wizard(args.bash, config, endpoint, [("Base URL [", ""), ("API Key (sk-...): ", KEY),
                       ("编号（必填）: ", str(i)), ("[y/N] ", "n")], env)
                provider_config = json.loads((config / "sudocode.json").read_text())
                settings = json.loads((config / "settings.json").read_text())
                assert settings == {"auth_profile": "sudorouter", "model": model}
                assert "models" not in provider_config
                if os.name != "nt":
                    assert (config / "sudocode.json").stat().st_mode & 0o777 == 0o600
                    assert (config / "settings.json").stat().st_mode & 0o777 == 0o600
                local = subprocess.run([str(args.scode.resolve()), "setup", "--mode", "local"],
                                       cwd=workspace, env=env, capture_output=True, timeout=30)
                assert local.returncode == 0, local.stderr.decode(errors="replace")
                result = subprocess.run([str(args.scode.resolve()), "--auth", "proxy", "--account", "sudorouter",
                                         "--permission-mode", "read-only", "-p", "Reply SETUP_OK"],
                                        cwd=workspace, env=env, capture_output=True, text=True, encoding="utf-8", timeout=45)
                assert result.returncode == 0 and "SETUP_OK" in result.stdout, (result.returncode, result.stdout, result.stderr, Provider.requests)
                expected_path = "/v1/messages" if i == 1 else "/v1/chat/completions"
                assert (expected_path, model) in Provider.requests, Provider.requests
                print(f"PASS: {model} uses {expected_path} through the generated account")
            config = root / "config-1"
            original = {n: (config / n).read_bytes() for n in ("sudocode.json", "settings.json")}
            wizard(args.bash, config, endpoint, [("[y/N] ", "n")], env)
            assert all((config / n).read_bytes() == data for n, data in original.items())
            wizard(args.bash, config, endpoint, [("[y/N] ", "y"), ("Base URL [", ""),
                   ("API Key (sk-...): ", "rejected-key")], env, expected=1)
            assert all((config / n).read_bytes() == data for n, data in original.items())
            wizard(args.bash, root / "invalid-choice", endpoint, [("Base URL [", ""),
                   ("API Key (sk-...): ", KEY), ("编号（必填）: ", "")], env, expected=1)
            assert not (root / "invalid-choice" / "sudocode.json").exists()
            wizard(args.bash, config, endpoint, [("[y/N] ", "y"), ("Base URL [", ""),
                   ("API Key (sk-...): ", KEY), ("编号（必填）: ", "2"), ("[y/N] ", "n")], env)
            for name, data in original.items():
                backup = list(config.glob(name + ".backup-*"))
                assert len(backup) == 1 and backup[0].read_bytes() == data
            print("PASS: cancellation, rejected key and invalid selection preserve configuration; replacement retains backups")
            print("Wizard transport:", "pipes (Windows supplementary coverage)" if os.name == "nt" else "PTY")
    finally:
        server.shutdown()
        server.server_close()
        thread.join()


if __name__ == "__main__":
    main()
