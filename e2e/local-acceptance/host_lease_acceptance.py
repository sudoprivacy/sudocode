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

    # Later checks wait for the holder, never interleave its profile, and take
    # the host in the order they arrived rather than in polling order.
    for label in ["B", "X", "Y"]:
        (fixture / f"release-{label}").touch()
    holder = start("A")
    wait_until(lambda: events() == ["start:A"], "the first check to hold the host")
    queued = []
    for label in ["B", "X", "Y"]:
        queued.append(start(label))
        wait_until(lambda: waiting(queued[-1]), f"check {label} to report the holder")
    assert "Queue position 3 of 3" in queued[-1].log.read_text(), queued[-1].log.read_text()
    assert events() == ["start:A"], events()
    # A check started under the holder is already serialized by it.
    (fixture / "release-C").touch()
    finished(start("C", nested=True))
    assert events() == ["start:A", "start:C", "end:C"], events()
    (fixture / "release-A").touch()
    finished(holder)
    for process in queued:
        finished(process)
    assert events() == ["start:A", "start:C", "end:C", "end:A", "start:B", "end:B",
                        "start:X", "end:X", "start:Y", "end:Y"], events()
    assert "Acquired this host's local acceptance lease" in queued[0].log.read_text()
    timing = json.loads((fixture / "B/attempt.json").read_text())
    assert timing["host_lease_wait_seconds"] > 0, timing
    assert not list((home / ".nexus/local-acceptance/queue").glob("*.ticket"))

    # A waiter killed in the queue does not block those behind it.
    events_log.unlink()
    (fixture / "release-F").touch()
    holder = start("D")
    wait_until(lambda: events() == ["start:D"], "the next check to hold the host")
    abandoned = start("E")
    wait_until(lambda: waiting(abandoned), "a check to queue behind the holder")
    queued = start("F")
    wait_until(lambda: waiting(queued), "a check to queue behind the killed waiter")
    abandoned.kill()
    abandoned.wait(timeout=60)
    (fixture / "release-D").touch()
    finished(holder)
    finished(queued)
    assert events() == ["start:D", "end:D", "start:F", "end:F"], events()

    # An abruptly killed holder releases the host without a stale lock.
    events_log.unlink()
    (fixture / "release-H").touch()
    holder = start("G")
    wait_until(lambda: events() == ["start:G"], "the next check to hold the host")
    queued = start("H")
    wait_until(lambda: waiting(queued), "a check to wait behind the next holder")
    holder.kill()
    holder.wait(timeout=60)
    finished(queued)
    assert events() == ["start:G", "start:H", "end:H"], events()
    # The killed runner's profile step outlives it; let it exit before cleanup.
    (fixture / "release-G").touch()
    wait_until(lambda: "end:G" in events(), "the killed holder's orphaned step to exit")
    print("PASS: one acceptance per host in arrival order, nested checks under the holder, "
          "release on abrupt holder or waiter exit")
