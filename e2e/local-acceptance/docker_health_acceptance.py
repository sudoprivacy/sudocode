"""Use the real Docker CLI against a deliberately stalled local HTTP endpoint."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import socketserver
import subprocess
import threading
import time

import psutil


SOURCE = Path(__file__).resolve().parents[2]


def run(output):
    output.mkdir(parents=True, exist_ok=False)
    docker = shutil.which("docker")
    if docker is None:
        raise RuntimeError("Docker health guard requires the actual Docker CLI")
    client = subprocess.run([docker, "--version"], capture_output=True, timeout=5, check=True)
    spec = importlib.util.spec_from_file_location("health_profiles", SOURCE / ".github/local-acceptance/checks.py")
    profiles = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(profiles)
    command = profiles.docker_engine_probe_command(timeout=2)
    assert command[-4:] == [docker, "version", "--format", "{{.Server.Version}}"]
    assert profiles.docker_engine_probe_command(30)[3] == "30"
    release = threading.Event()
    accepted = threading.Event()

    class Stall(socketserver.BaseRequestHandler):
        def handle(self):
            accepted.set()
            release.wait(10)

    class Server(socketserver.ThreadingTCPServer):
        daemon_threads = True

    server = Server(("127.0.0.1", 0), Stall)
    thread = threading.Thread(target=lambda: server.serve_forever(poll_interval=0.05), daemon=True)
    thread.start()
    config = output / "docker-config"
    config.mkdir()
    env = dict(os.environ)
    for name in ["DOCKER_CONTEXT", "DOCKER_HOST", "DOCKER_TLS", "DOCKER_TLS_VERIFY", "DOCKER_CERT_PATH"]:
        env.pop(name, None)
    env.update(DOCKER_CONFIG=str(config), DOCKER_HOST=f"tcp://127.0.0.1:{server.server_address[1]}")
    start = time.monotonic()
    process = subprocess.Popen(command, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    timed_out = False
    try:
        raw, _ = process.communicate(timeout=8)
    except subprocess.TimeoutExpired:
        timed_out = True
        # This external watchdog is a failing control, never a passing timeout.
        # Stop only descendants of this fixture's exact live wrapper process.
        owner = psutil.Process(process.pid)
        for child in reversed(owner.children(recursive=True)):
            try:
                child.terminate()
            except psutil.NoSuchProcess:
                pass
        process.terminate()
        raw, _ = process.communicate(timeout=5)
    finally:
        elapsed = time.monotonic() - start
        release.set()
        server.shutdown()
        server.server_close()
        thread.join(timeout=2)
    (output / "probe.log").write_bytes(raw)
    record = {"command": command, "exit_code": process.returncode, "elapsed_seconds": elapsed,
              "external_watchdog_fired": timed_out, "actual_http_connection_accepted": accepted.is_set(),
              "log_sha256": hashlib.sha256(raw).hexdigest(), "docker_client": client.stdout.decode().strip(),
              "scope": "Real Docker CLI, controlled stalled TCP endpoint and production probe command. "
                       "Connection timeout only; no healthy Engine or product/performance coverage inferred."}
    (output / "result.json").write_text(json.dumps(record, indent=2) + "\n", encoding="utf-8")
    assert not timed_out, "The production connection probe exceeded the external failing watchdog"
    assert accepted.is_set(), "The actual Docker client never reached the controlled stalled endpoint"
    assert process.returncode != 0 and b"DOCKER_ENGINE_PROBE_TIMEOUT" in raw, raw
    env["DOCKER_CONTEXT"] = "missing-context"
    refused = subprocess.run(command, env=env, capture_output=True, timeout=8)
    (output / "missing-context.log").write_bytes(refused.stdout + refused.stderr)
    assert refused.returncode != 0 and b"DOCKER_ENGINE_PROBE_TIMEOUT" not in refused.stderr
    record["original_client_error_preserved"] = True
    record["status"] = "PASS"
    (output / "result.json").write_text(json.dumps(record, indent=2) + "\n", encoding="utf-8")
    print(f"Docker health connection deadline refused a live stalled endpoint in {elapsed:.3f}s: {output}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    run(parser.parse_args().output.resolve())
