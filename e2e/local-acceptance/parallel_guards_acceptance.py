"""Exercise independent native processes and real Docker failure/interrupt cleanup."""

import argparse
from datetime import datetime, timezone
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import time
import uuid

import psutil


SOURCE = Path(__file__).resolve().parents[2]
DOCKER_PROGRAM = """import json,pathlib,sys,time
p=pathlib.Path('/field')
(p/'docker-started.json').write_text(json.dumps({'started':True}))
end=time.monotonic()+30
while not (p/'native.pid.json').exists():
 if time.monotonic()>end: raise SystemExit(31)
 time.sleep(.05)
if sys.argv[1]=='docker-failure':
 time.sleep(1); raise SystemExit(23)
if sys.argv[1]=='clean': raise SystemExit(0)
time.sleep(30)
"""
NATIVE_PROGRAM = """import json,os,pathlib,subprocess,sys,time,psutil
p=pathlib.Path(sys.argv[1]);mode=sys.argv[2]
child=subprocess.Popen([sys.executable,'-c','import time;time.sleep(30)'])
(p/'native.pid.pending').write_text(json.dumps({'pid':os.getpid(),'created':psutil.Process().create_time(),
 'child':child.pid,'child_created':psutil.Process(child.pid).create_time()}))
(p/'native.pid.pending').replace(p/'native.pid.json')
end=time.monotonic()+30
while not (p/'docker-started.json').exists():
 if time.monotonic()>end: raise SystemExit(32)
 time.sleep(.05)
if mode in ['clean','native-failure']:
 time.sleep(1);child.terminate();child.wait()
 raise SystemExit(17 if mode=='native-failure' else 0)
time.sleep(30)
"""


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def gone(pid, created):
    try:
        process = psutil.Process(pid)
        return process.create_time() != created or not process.is_running()
    except psutil.NoSuchProcess:
        return True


def run(output, image):
    output.mkdir(parents=True, exist_ok=False)
    docker = shutil.which("docker")
    assert docker
    helper = load("parallel_fixture_authority", SOURCE / ".github/scripts/local_acceptance.py")
    runner_path = SOURCE / ".github/local-acceptance/parallel_guard.py"
    runner = load("parallel_fixture_runner", runner_path)
    helper.ROOT = SOURCE
    summary = {"started_at": datetime.now(timezone.utc).isoformat(), "cases": [], "status": "running"}
    started = time.monotonic()
    attempt = {"active_profile": "private-parallel-guard-failure-controls"}
    try:
        with helper.host_acceptance_lease(attempt, "parallel-guard-private-native-docker-controls"):
            for mode in ["clean", "docker-failure", "native-failure", "interrupted"]:
                directory = output / mode
                directory.mkdir()
                owner = uuid.uuid4().hex
                configuration = directory / "commands.json"
                data = {"version": 1, "root": str(SOURCE), "container": {
                    "docker": docker, "cidfile": str(directory / "container.id"), "image": image, "owner": owner,
                }, "branches": [
                    {"name": "docker", "command": [docker, "run", "--rm", "--init", "--cpus", "2",
                        "--memory", "2g", "--pids-limit", "256", "--cidfile", str(directory / "container.id"),
                        "--label", f"{runner.LABEL}={owner}", "--mount", f"type=bind,source={directory},target=/field",
                        "--entrypoint", "python3", image, "-c", DOCKER_PROGRAM, mode],
                     "log": str(directory / "docker.log")},
                    {"name": "native", "command": [sys.executable, "-c", NATIVE_PROGRAM, str(directory), mode],
                     "log": str(directory / "native.log")},
                ]}
                configuration.write_text(json.dumps(data, indent=2) + "\n", encoding="utf-8")
                begin = time.monotonic()
                process = None
                try:
                    with (directory / "controller.log").open("wb") as log:
                        process = subprocess.Popen([sys.executable, str(runner_path), "--configuration", str(configuration)],
                            cwd=SOURCE, stdout=log, stderr=subprocess.STDOUT,
                            creationflags=subprocess.CREATE_NEW_PROCESS_GROUP if os.name == "nt" else 0)
                        marker = directory / "native.pid.json"
                        deadline = time.monotonic() + 30
                        while not marker.exists() or not (directory / "docker-started.json").exists():
                            assert process.poll() is None, directory / "controller.log"
                            assert time.monotonic() < deadline, "Both actual peer workloads must start"
                            time.sleep(.05)
                        identity = json.loads(marker.read_text())
                        if mode == "interrupted":
                            process.send_signal(signal.CTRL_BREAK_EVENT if os.name == "nt" else signal.SIGINT)
                        code = process.wait(timeout=30)
                    record = json.loads(configuration.with_suffix(".result.json").read_text())
                    elapsed = time.monotonic() - begin
                    assert elapsed < 30, "A cancelled peer must not complete its original sleep"
                    assert record["status"] == ("passed" if mode == "clean" else "failed")
                    assert (code == 0) == (mode == "clean")
                    if mode in ["docker-failure", "native-failure"]:
                        branch = mode.split("-")[0]
                        assert record["first_failed_branch"] == branch
                        original = json.loads((directory / f"{branch}.command.json").read_text())
                        assert original["exit_code"] == (23 if branch == "docker" else 17)
                    assert gone(identity["pid"], identity["created"])
                    assert gone(identity["child"], identity["child_created"]), "Native child must not survive cancellation"
                    assert runner.cleanup_container(data)["state"] == "already_removed"
                    summary["cases"].append({"mode": mode, "exit_code": code, "seconds": elapsed,
                        "both_real_workloads_started": True, "native_parent_and_child_gone": True,
                        "exact_owned_container_removed": True})
                finally:
                    if process is not None and process.poll() is None:
                        process.terminate()
                        process.wait(timeout=10)
                    runner.cleanup_container(data)
            for mode in ["wrong-owner", "wrong-image", "empty-id-cleanup"]:
                directory = output / mode
                directory.mkdir()
                owner = uuid.uuid4().hex
                cidfile = directory / "container.id"
                data = {"container": {"docker": docker, "cidfile": str(cidfile), "image": image, "owner": owner}}
                begin = time.monotonic()
                try:
                    created = subprocess.run([docker, "run", "--detach", "--rm", "--init", "--cpus", "2",
                        "--memory", "2g", "--pids-limit", "256", "--cidfile", str(cidfile),
                        "--label", f"{runner.LABEL}={owner}", "--entrypoint", "python3", image,
                        "-c", "import time;time.sleep(30)"], capture_output=True, timeout=30, check=True)
                    (directory / "original-create.log").write_bytes(created.stdout + created.stderr)
                    identity = cidfile.read_text().strip()
                    if mode == "empty-id-cleanup":
                        cidfile.write_text("")
                        assert runner.cleanup_container(data)["id"] == identity
                    else:
                        other = json.loads(json.dumps(data))
                        other["container"]["owner" if mode == "wrong-owner" else "image"] = (
                            uuid.uuid4().hex if mode == "wrong-owner" else "sha256:" + "0" * 64)
                        refused = False
                        try:
                            runner.cleanup_container(other)
                        except AssertionError:
                            refused = True
                        assert refused, "Mismatched ownership/image must not permit removal"
                        still_live = subprocess.run([docker, "inspect", identity], capture_output=True, timeout=30, check=True)
                        assert json.loads(still_live.stdout)[0]["State"]["Running"] is True
                        runner.cleanup_container(data)
                    absent = subprocess.run([docker, "inspect", identity], capture_output=True, timeout=30)
                    assert runner.missing_container(absent, identity)
                    summary["cases"].append({"mode": mode, "seconds": time.monotonic() - begin,
                        "actual_engine_identity": identity, "exact_owned_container_removed": True})
                finally:
                    runner.cleanup_container(data)
        summary["status"] = "PASS"
    except BaseException as error:
        summary.update(status="FAIL", error=repr(error))
        raise
    finally:
        summary.update(finished_at=datetime.now(timezone.utc).isoformat(), seconds=time.monotonic() - started, lease=attempt,
            scope="Actual Docker CLI, native parent/child processes, original command failures and host-native interrupt. Private workloads only; no product/provider or cross-platform coverage.")
        (output / "result.json").write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(summary), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--image", required=True)
    args = parser.parse_args()
    run(args.output.resolve(), args.image)
