"""Exercise genuine lease inheritance and refusals through native child processes."""

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import time


LEASE_ENV = "SUDOCODE_ACCEPTANCE_HOST_LEASE"
SOURCE = Path(__file__).resolve().parents[2]

ACTOR = '''import errno, importlib.util, json, os, subprocess, sys, time
from pathlib import Path
source, output = map(Path, sys.argv[1:3])
mode, label = sys.argv[3:5]
spec = importlib.util.spec_from_file_location("lease_authority", source / ".github/scripts/local_acceptance.py")
authority = importlib.util.module_from_spec(spec)
spec.loader.exec_module(authority)
authority.ROOT = source
lease_env = authority.HOST_LEASE_ENV
lock = Path.home() / ".nexus/local-acceptance/host.lock"
if mode == "enter":
    with authority.host_acceptance_lease({}, "fixture-child"):
        (output / (label + ".entered")).touch()
    raise SystemExit(0)

checks = []
def child(label, claim, success):
    command = [sys.executable, __file__, str(source), str(output), "enter", label]
    result = subprocess.run(command, env={**os.environ, lease_env: claim}, capture_output=True, timeout=15)
    (output / (label + ".log")).write_bytes(result.stdout + result.stderr)
    assert (result.returncode == 0) == success, (label, result.stdout, result.stderr)
    assert (output / (label + ".entered")).exists() == success, label
    if not success:
        assert b"Invalid inherited host acceptance lease" in result.stderr, result.stderr
    checks.append(label)

def replace_record(record):
    descriptor = os.open(lock, os.O_RDWR)
    try:
        authority.write_host_lease_record(descriptor, record)
    finally:
        os.close(descriptor)

with authority.host_acceptance_lease({}, "fixture-owner"):
    claim = os.environ[lease_env]
    with authority.host_acceptance_lease({}, "same-process-nesting"):
        assert os.environ[lease_env] == claim
    checks.append("same-process-nesting")
    child("genuine-descendant", claim, True)
    descriptor = os.open(lock, os.O_RDWR)
    try:
        record = authority.read_host_lease_record(descriptor)
    finally:
        os.close(descriptor)
    child("wrong-acquisition", claim[:-1] + ("0" if claim[-1] != "0" else "1"), False)
    replace_record({**record, "process_created_at": record["process_created_at"] - 1})
    child("wrong-process-incarnation", claim, False)
    replace_record(record)
    observer = lock.with_name("host-lease.json")
    observer.write_text("non-authoritative diagnostic copy")
    child("locked-record-is-authoritative", claim, True)
    observer.write_bytes(authority.canonical(record))
    ready = output / "owner-ready.json"
    ready.with_suffix(".tmp").write_text(json.dumps({"claim": claim, "pid": os.getpid()}))
    ready.with_suffix(".tmp").replace(ready)
    deadline = time.monotonic() + 45
    while not (output / "release-owner").exists() and time.monotonic() < deadline:
        time.sleep(.02)
    assert (output / "release-owner").exists(), "controller never released its private owner"
child("released-acquisition", claim, False)
# An active-looking record cannot substitute for the actual OS lock, even
# while the original owner process and its birth time still match.
replace_record(record)
child("unlocked-record", claim, False)
with authority.host_acceptance_lease({}, "fixture-second-acquisition"):
    child("previous-acquisition-same-live-owner", claim, False)
    child("new-acquisition-descendant", os.environ[lease_env], True)
descriptor = os.open(lock, os.O_RDWR)
os.close(descriptor)
try:
    authority.try_host_lock(descriptor)
except OSError as error:
    assert error.errno == errno.EBADF, error
else:
    raise AssertionError("A real descriptor error must not be reported as a held lock")
checks.append("descriptor-error-refused")
(output / "owner-results.json").write_text(json.dumps(checks))
'''


def verify_inherited_lease(output, source, environment):
    """Use private lock files and real ancestry; never touch a live host lease."""
    output.mkdir(parents=True, exist_ok=False)
    home = output / "home"
    home.mkdir()
    environment = {key: value for key, value in environment.items() if key != LEASE_ENV}
    environment.update(HOME=str(home), USERPROFILE=str(home))
    actor = output / "actor.py"
    actor.write_text(ACTOR, encoding="utf-8")
    command = [sys.executable, str(actor), str(source), str(output)]
    log = (output / "owner.log").open("wb")
    owner = subprocess.Popen([*command, "owner", "owner"], env=environment, stdout=log, stderr=subprocess.STDOUT)
    external = []
    try:
        ready = output / "owner-ready.json"
        deadline = time.monotonic() + 30
        while not ready.is_file():
            assert owner.poll() is None, (output / "owner.log").read_text(errors="replace")
            assert time.monotonic() < deadline, "Private lease owner did not become ready"
            time.sleep(.02)
        claim = json.loads(ready.read_text())["claim"]
        for label, value in [("arbitrary-string", "enclosing-acceptance"),
                             ("copied-unrelated-owner", claim)]:
            result = subprocess.run([*command, "enter", label], env={**environment, LEASE_ENV: value},
                                    capture_output=True, timeout=15)
            (output / (label + ".log")).write_bytes(result.stdout + result.stderr)
            assert result.returncode != 0 and not (output / (label + ".entered")).exists(), label
            assert b"Invalid inherited host acceptance lease" in result.stderr, result.stderr
            assert owner.poll() is None, "Negative control must run while the real owner holds the lock"
            external.append(label)
        (output / "release-owner").touch()
        assert owner.wait(timeout=30) == 0, (output / "owner.log").read_text(errors="replace")
        checks = json.loads((output / "owner-results.json").read_text()) + external
        assert len(checks) == 12, checks
        (output / "receipt.json").write_text(json.dumps({
            "status": "passed", "system": sys.platform, "real_native_processes": True,
            "private_home": str(home), "checks": checks,
        }, indent=2) + "\n", encoding="utf-8")
        print("PASS: real parent/child and same-process inheritance; arbitrary, copied, wrong-incarnation, "
              "old-acquisition and unlocked claims refused; diagnostic copy is not authority")
    finally:
        (output / "release-owner").touch()
        if owner.poll() is None:
            try:
                owner.wait(timeout=10)
            except subprocess.TimeoutExpired:
                owner.terminate()
                owner.wait(timeout=10)
        log.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    verify_inherited_lease(args.output.resolve(), SOURCE, os.environ)
