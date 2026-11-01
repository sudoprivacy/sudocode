"""Exercise the host acceptance lease through concurrent CLI processes, Git and SSH."""

import json
from pathlib import Path
import shutil
import subprocess
import sys
import time

LEASE_ENV = "SUDOCODE_ACCEPTANCE_HOST_LEASE"


def verify_host_lease(directory, source, command, environment):
    fixture = directory / "host-lease"
    fixture.mkdir()
    work, home = fixture / "work", fixture / "home"
    home.mkdir()

    def run(*args, cwd=work, **kwargs):
        return command(*args, cwd=cwd, **kwargs)

    run("git", "init", "--initial-branch=main", work, cwd=fixture)
    run("git", "config", "user.name", "Host Lease E2E")
    run("git", "config", "user.email", "host-lease@example.invalid")
    helper = work / ".github/scripts/local_acceptance.py"
    helper.parent.mkdir(parents=True)
    shutil.copyfile(source / ".github/scripts/local_acceptance.py", helper)
    profiles = work / ".github/local-acceptance/checks.py"
    profiles.parent.mkdir()
    # Each check records when its only profile starts and ends, and holds the
    # host until its release file exists.
    profiles.write_text(
        'import sys\n'
        'def run(name, root, output, base, execute):\n'
        '    label = output.parent.name\n'
        '    code = ("from pathlib import Path; import sys, time; events = Path(sys.argv[1]); '
        'events.open(\'a\').write(\'start:\' + sys.argv[3] + chr(10)); '
        'deadline = time.monotonic() + 120\\n'
        'while not Path(sys.argv[2]).exists() and time.monotonic() < deadline: time.sleep(0.05)\\n'
        'assert Path(sys.argv[2]).exists(), \'release never arrived\'\\n'
        'events.open(\'a\').write(\'end:\' + sys.argv[3] + chr(10))")\n'
        '    return [execute([sys.executable, "-c", code, str(root.parent / "events.log"), '
        'str(root.parent / ("release-" + label)), label], root, output / "000.log")]\n'
    )
    (profiles.parent / "Dockerfile").write_text("FROM scratch\n")
    workflows = work / ".github/workflows"
    workflows.mkdir()
    for name in ["local-acceptance.yml", "local-acceptance-publish.yml"]:
        (workflows / name).write_text("# Local enforcement fixture\n")
    (work / "spec").mkdir()
    (work / "spec/local_acceptance.json").write_text(json.dumps({
        "version": 1, "repository": "acceptance/host-lease-fixture",
        "checks": {"native-workspace": {"include": ["input.txt"]}},
    }))
    key = fixture / "device-key"
    run("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", key)
    system = {"win32": "windows", "darwin": "macos"}.get(sys.platform, "linux")
    (work / "spec/local_acceptance_signers.json").write_text(json.dumps({
        "version": 1, "signers": {"fixture": {
            "platform": system, "public_key": key.with_suffix(".pub").read_text().strip(),
        }},
    }))
    run("git", "config", "localAcceptance.signer", "fixture")
    run("git", "config", "localAcceptance.key", key)
    (work / ".gitignore").write_text("__pycache__/\n")
    (work / "input.txt").write_text("initial\n")
    run("git", "add", ".")
    run("git", "commit", "-m", "Protected host lease fixture")
    run("git", "checkout", "-b", "feature")
    (work / "input.txt").write_text("candidate\n")
    run("git", "add", ".")
    run("git", "commit", "-m", "Candidate input")

    # A private home isolates this fixture from a real acceptance holding the
    # host, and the lease variable an enclosing acceptance passes down is cleared.
    isolated = {key: value for key, value in environment.items() if key != LEASE_ENV}
    isolated.update(HOME=str(home), USERPROFILE=str(home))
    events_log = fixture / "events.log"

    def events():
        return events_log.read_text().splitlines() if events_log.exists() else []

    def start(label, nested=False):
        log = (fixture / f"{label}.out").open("w")
        env = {**isolated, LEASE_ENV: "enclosing-acceptance"} if nested else isolated
        process = subprocess.Popen(
            [sys.executable, str(helper), "check", "--base", "main", "--policy-ref", "main",
             "--output", str(fixture / label)],
            cwd=work, env=env, stdout=log, stderr=subprocess.STDOUT)
        process.log = fixture / f"{label}.out"
        return process

    def wait_until(condition, description, timeout=120):
        deadline = time.monotonic() + timeout
        while not condition():
            assert time.monotonic() < deadline, f"Timed out waiting for {description}: {events()}"
            time.sleep(0.05)

    def finished(process):
        assert process.wait(timeout=120) == 0, process.log.read_text()
        assert "Local acceptance complete" in process.log.read_text(), process.log.read_text()

    def waiting(process):
        return "Waiting for this host's local acceptance lease" in process.log.read_text()

    # A second check waits for the holder and never interleaves its profile.
    (fixture / "release-B").touch()
    holder = start("A")
    wait_until(lambda: events() == ["start:A"], "the first check to hold the host")
    queued = start("B")
    wait_until(lambda: waiting(queued), "the second check to report the holder")
    assert events() == ["start:A"], events()
    # A check started under the holder is already serialized by it.
    (fixture / "release-C").touch()
    finished(start("C", nested=True))
    assert events() == ["start:A", "start:C", "end:C"], events()
    (fixture / "release-A").touch()
    finished(holder)
    finished(queued)
    assert events() == ["start:A", "start:C", "end:C", "end:A", "start:B", "end:B"], events()
    assert "Acquired this host's local acceptance lease" in queued.log.read_text()
    timing = json.loads((fixture / "B/attempt.json").read_text())
    assert timing["host_lease_wait_seconds"] > 0, timing

    # An abruptly killed holder releases the host without a stale lock.
    events_log.unlink()
    (fixture / "release-E").touch()
    holder = start("D")
    wait_until(lambda: events() == ["start:D"], "the next check to hold the host")
    queued = start("E")
    wait_until(lambda: waiting(queued), "a check to wait behind the next holder")
    holder.kill()
    holder.wait(timeout=60)
    finished(queued)
    assert events() == ["start:D", "start:E", "end:E"], events()
    # The killed runner's profile step outlives it; let it exit before cleanup.
    (fixture / "release-D").touch()
    wait_until(lambda: "end:D" in events(), "the killed holder's orphaned step to exit")
    print("PASS: one acceptance per host, nested checks under the holder, release on abrupt exit")
