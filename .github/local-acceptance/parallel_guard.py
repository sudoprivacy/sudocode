"""Supervise two independent guard commands using the original command executor."""

import argparse
import importlib.util
import json
from pathlib import Path
import re
import signal
import subprocess
import sys
import time

import psutil


LABEL = "scode.contract.guard"


def missing_container(result, identity):
    """Accept only Docker's absent-object diagnostic for this exact identity."""
    error = result.stderr.lower()
    return (result.returncode != 0 and identity.encode() in error
            and (b"no such object" in error or b"no such container" in error))


def read_configuration(path):
    """Require private, distinct evidence paths before starting either branch."""
    if path.is_symlink() or not path.is_file():
        raise RuntimeError("Parallel guard configuration must be a regular file")
    data = json.loads(path.read_text(encoding="utf-8"))
    assert data["version"] == 1 and len(data["branches"]) == 2
    assert {row["name"] for row in data["branches"]} == {"docker", "native"}
    logs = [Path(row["log"]) for row in data["branches"]]
    assert len(set(logs)) == 2
    assert all(log.resolve().parent == path.resolve().parent and not log.is_symlink() for log in logs)
    assert all(isinstance(arg, str) for row in data["branches"] for arg in row["command"])
    container = data["container"]
    cidfile = Path(container["cidfile"])
    assert cidfile.resolve().parent == path.resolve().parent and not cidfile.is_symlink()
    assert re.fullmatch(r"sha256:[a-f0-9]{64}", container["image"])
    assert re.fullmatch(r"[a-f0-9]{32}", container["owner"])
    return data


def cleanup_container(data):
    """Remove only the previously created container with this image and owner label."""
    container = data["container"]
    docker = container["docker"]
    cidfile = Path(container["cidfile"])
    identity = cidfile.read_text().strip() if cidfile.exists() else ""
    if not re.fullmatch(r"[a-f0-9]{64}", identity):
        # Docker opens --cidfile before writing its ID. An interrupted client
        # can leave it empty, so find only this invocation's random owner label.
        found = subprocess.run([docker, "ps", "--all", "--quiet", "--no-trunc", "--filter",
            f"label={LABEL}={container['owner']}"], capture_output=True, timeout=30, check=True)
        identities = found.stdout.decode().split()
        assert len(identities) <= 1 and all(re.fullmatch(r"[a-f0-9]{64}", value) for value in identities)
        if not identities:
            return {"state": "no_current_container_with_owned_label", "owner": container["owner"]}
        identity = identities[0]
    inspected = subprocess.run([docker, "inspect", identity], capture_output=True, timeout=30)
    if inspected.returncode:
        if missing_container(inspected, identity):
            return {"id": identity, "state": "already_removed"}
        raise RuntimeError(f"Cannot inspect owned guard container: {inspected.stderr.decode(errors='replace')}")
    records = json.loads(inspected.stdout)
    assert len(records) == 1
    actual = records[0]
    assert actual["Id"] == identity and actual["Image"] == container["image"]
    assert actual["Config"]["Labels"].get(LABEL) == container["owner"]
    removed = subprocess.run([docker, "rm", "--force", identity], capture_output=True, timeout=30)
    if removed.returncode:
        # AutoRemove may finish between the authenticated inspect and removal.
        in_progress = identity.encode() in removed.stderr and b"already in progress" in removed.stderr.lower()
        if not missing_container(removed, identity) and not in_progress:
            raise RuntimeError(f"Cannot remove owned guard container: {removed.stderr.decode(errors='replace')}")
    deadline = time.monotonic() + 30
    while True:
        absent = subprocess.run([docker, "inspect", identity], capture_output=True, timeout=30)
        if missing_container(absent, identity):
            break
        if absent.returncode or time.monotonic() >= deadline:
            raise RuntimeError(f"Owned guard container removal is unverified: {identity}")
        time.sleep(.1)
    return {"id": identity, "state": "removed_after_image_and_owner_verification"}


def worker(data, name):
    """Keep command logs, compiler priorities and refusal behavior in one executor."""
    helper_path = Path(__file__).resolve().parents[1] / "scripts/local_acceptance.py"
    spec = importlib.util.spec_from_file_location("parallel_guard_command_executor", helper_path)
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    helper.ROOT = Path(data["root"])
    branch = next(row for row in data["branches"] if row["name"] == name)
    helper.run_command(branch["command"], helper.ROOT, Path(branch["log"]))


def terminate_workers(processes, observed):
    """Stop only verified children started by this invocation, including native descendants."""
    owned = list(observed.values())
    for process in processes.values():
        if process.poll() is None:
            try:
                owned.extend(psutil.Process(process.pid).children(recursive=True))
            except psutil.NoSuchProcess:
                pass
    for child in reversed(owned):
        try:
            if child.is_running():
                child.terminate()
        except psutil.NoSuchProcess:
            pass
    for process in processes.values():
        if process.poll() is None:
            process.terminate()
    _gone, alive = psutil.wait_procs(owned, timeout=5)
    for child in alive:
        if child.is_running():
            child.kill()
    for process in processes.values():
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def run(path, data):
    """Report the first failure immediately and clean up both owned branches."""
    processes, observed = {}, {}
    outcome = {"version": 1, "status": "running", "started_at": time.time(), "branches": {}}
    try:
        # Start the original docker run first. Native work begins only after
        # Docker records its exact identity, so sibling failure can stop it.
        ordered = sorted(data["branches"], key=lambda row: row["name"] != "docker")
        for branch in ordered:
            processes[branch["name"]] = subprocess.Popen(
                [sys.executable, str(Path(__file__).resolve()), "--configuration", str(path), "--worker", branch["name"]],
                cwd=data["root"],
            )
            if branch["name"] == "docker":
                cidfile = Path(data["container"]["cidfile"])
                while True:
                    identity = cidfile.read_text().strip() if cidfile.exists() else ""
                    if re.fullmatch(r"[a-f0-9]{64}", identity):
                        break
                    code = processes["docker"].poll()
                    if code is not None:
                        raise RuntimeError(f"Original Docker guard exited before recording its identity ({code})")
                    time.sleep(0.05)
        pending = set(processes)
        while pending:
            for name in list(pending):
                process = processes[name]
                try:
                    for child in psutil.Process(process.pid).children(recursive=True):
                        observed[(child.pid, child.create_time())] = child
                except psutil.NoSuchProcess:
                    pass
                code = process.poll()
                if code is None:
                    continue
                outcome["branches"][name] = {"exit_code": code, "finished_at": time.time()}
                pending.remove(name)
                if code:
                    outcome["first_failed_branch"] = name
                    print(f"PARALLEL_GUARD_FAILED: {name}; cancelling only this invocation's other guard. Original logs remain required.",
                          flush=True)
                    raise RuntimeError(f"Original {name} contract guard failed ({code})")
            time.sleep(0.15)
        outcome["status"] = "passed"
    except BaseException as error:
        outcome.update(status="failed", error=repr(error))
        raise
    finally:
        try:
            terminate_workers(processes, observed)
            outcome["container_cleanup"] = cleanup_container(data)
        except BaseException as error:
            outcome.update(status="failed", cleanup_error=repr(error))
            raise
        finally:
            outcome["finished_at"] = time.time()
            path.with_suffix(".result.json").write_text(json.dumps(outcome, indent=2) + "\n", encoding="utf-8")


def interrupted(_signal, _frame):
    raise KeyboardInterrupt


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--configuration", type=Path, required=True)
    parser.add_argument("--worker", choices=["docker", "native"])
    parser.add_argument("--cleanup", action="store_true")
    arguments = parser.parse_args()
    configuration = arguments.configuration.resolve()
    data = read_configuration(configuration)
    if arguments.cleanup:
        print(json.dumps(cleanup_container(data)), flush=True)
    elif arguments.worker:
        worker(data, arguments.worker)
    else:
        signal.signal(signal.SIGTERM, interrupted)
        if hasattr(signal, "SIGBREAK"):
            signal.signal(signal.SIGBREAK, interrupted)
        run(configuration, data)
