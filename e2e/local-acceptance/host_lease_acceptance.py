"""Exercise the host acceptance lease through concurrent CLI processes, Git and SSH."""

import hashlib
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
        '    if name == "preflight":\n'
        '        code = ("from pathlib import Path; import sys; "\n'
        '                "Path(sys.argv[1]).open(\'a\').write(sys.argv[2] + chr(10)); "\n'
        '                "note = Path(sys.argv[3]); "\n'
        '                "sys.exit(\'METADATA_FAILURE\' if note.is_file() and note.read_text().strip() == \'fail\' else 0)")\n'
        '        return [execute([sys.executable, "-c", code, str(root.parent / "metadata.log"), label, '
        'str(root / "notes.txt")], root, output / "000.log")]\n'
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
        "checks": {"preflight": {"include": ["*"]}, "native-workspace": {"include": ["input.txt"]}},
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

    metadata = fixture / "metadata-source"
    run("git", "worktree", "add", "--detach", metadata, "main")
    (metadata / "notes.txt").write_text("metadata-only change\n")
    run("git", "add", "notes.txt", cwd=metadata)
    run("git", "commit", "-m", "Metadata-only candidate", cwd=metadata)
    rejected = fixture / "rejected-source"
    run("git", "worktree", "add", "--detach", rejected, "feature")
    (rejected / "notes.txt").write_text("fail\n")
    run("git", "add", "notes.txt", cwd=rejected)
    run("git", "commit", "-m", "Metadata failure before native input", cwd=rejected)

    # A private home isolates this fixture from a real acceptance holding the
    # host, and the lease variable an enclosing acceptance passes down is cleared.
    isolated = {key: value for key, value in environment.items() if key != LEASE_ENV}
    isolated.update(HOME=str(home), USERPROFILE=str(home))
    events_log = fixture / "events.log"

    def events():
        return events_log.read_text().splitlines() if events_log.exists() else []

    processes = []
    sources = {}

    def start(label, nested=False, root=work, resume=None):
        # These exercise fresh executions, so each independent contender gets
        # its own immutable source head. Exact-head reuse is tested separately.
        if root == work:
            root = fixture / ("source-" + label)
            run("git", "worktree", "add", "--detach", root, "feature")
            (root / "input.txt").write_text("candidate " + label + "\n", encoding="utf-8")
            run("git", "commit", "-am", "Candidate " + label, cwd=root)
        sources[label] = root
        log = (fixture / f"{label}.out").open("w")
        env = {**isolated, LEASE_ENV: "enclosing-acceptance"} if nested else isolated
        args = [sys.executable, str(root / ".github/scripts/local_acceptance.py"), "check",
                "--base", "main", "--policy-ref", "main", "--output", str(fixture / label)]
        if resume is not None:
            args += ["--resume-from", str(resume)]
        process = subprocess.Popen(args, cwd=root, env=env, stdout=log, stderr=subprocess.STDOUT)
        log.close()
        processes.append(process)
        process.log = fixture / f"{label}.out"
        return process

    def wait_until(condition, description, timeout=120):
        deadline = time.monotonic() + timeout
        while not condition():
            assert time.monotonic() < deadline, f"Timed out waiting for {description}: {events()}"
            time.sleep(0.05)

    def finished(process, timeout=120):
        assert process.wait(timeout=timeout) == 0, process.log.read_text()
        assert "Local acceptance complete" in process.log.read_text(), process.log.read_text()

    def waiting(process):
        return "Waiting for this host's local acceptance lease" in process.log.read_text()

    try:
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
        # Mixed checks do their metadata work before joining the heavy FIFO.
        metadata_log = fixture / "metadata.log"
        wait_until(lambda: "B" in metadata_log.read_text().splitlines(),
                   "queued mixed-plan metadata to complete before the heavy holder exits", timeout=10)
        # A metadata-only revision can finish and sign while native work holds the host.
        finished(start("Metadata", root=metadata))
        meta_attempt = json.loads((fixture / "Metadata/attempt.json").read_text())
        assert meta_attempt["host_lease_wait_seconds"] == 0, meta_attempt
        assert not waiting(processes[-1]), processes[-1].log.read_text()
        assert (fixture / "Metadata/envelope.json").is_file()
        assert events() == ["start:A"], events()
        # A metadata failure on a mixed plan must refuse before any native command.
        failure = start("Bad", root=rejected)
        assert failure.wait(timeout=15) != 0, failure.log.read_text()
        assert "METADATA_FAILURE" in failure.log.read_text(), failure.log.read_text()
        assert not (fixture / "Bad/envelope.json").exists()
        assert not (fixture / "Bad/native-workspace").exists()
        assert not waiting(failure), failure.log.read_text()
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
        # Fully verified exact-head progress needs no native slot to re-sign.
        restored = start("Reused", root=sources["A"])
        finished(restored, timeout=15)
        restored_timing = json.loads((fixture / "Reused/attempt.json").read_text())
        assert restored_timing["host_lease_wait_seconds"] == 0, restored_timing
        assert not waiting(restored), restored.log.read_text()
        assert events() == ["start:G"], events()
        # A later commit retains the original signed native result when only
        # metadata changed, even though that original output was external.
        (sources["A"] / "notes.txt").write_text("later metadata\n", encoding="utf-8")
        run("git", "add", "notes.txt", cwd=sources["A"])
        run("git", "commit", "-m", "Metadata after external acceptance", cwd=sources["A"])
        carried = start("Carried", root=sources["A"])
        finished(carried, timeout=15)
        carried_receipt = json.loads((fixture / "Carried/receipt.json").read_text(encoding="utf-8"))
        assert carried_receipt["carried"][0]["profiles"] == ["native-workspace"], carried_receipt
        assert events() == ["start:G"], events()
        # A signed preflight checkpoint alone cannot skip the pending native step.
        (fixture / "release-Partial").touch()
        partial = start("Partial", root=sources["E"], resume=fixture / "E")
        wait_until(lambda: waiting(partial), "partial signed progress to retain native serialization")
        assert not (fixture / "Partial/envelope.json").exists()
        assert events() == ["start:G"], events()
        holder.kill()
        holder.wait(timeout=60)
        finished(queued)
        finished(partial)
        assert events() == ["start:G", "start:H", "end:H", "start:Partial", "end:Partial"], events()
        # The killed runner's profile step outlives it; let it exit before cleanup.
        (fixture / "release-G").touch()
        wait_until(lambda: "end:G" in events(), "the killed holder's orphaned step to exit")
        print("PASS: one acceptance per host in arrival order, nested checks under the holder, "
              "release on abrupt holder or waiter exit; metadata-only completion and mixed-plan preflight success/failure and verified full/partial resume scheduling")
    finally:
        # A negative control must release its own private waits as well.
        for label in ["A", "B", "X", "Y", "C", "D", "E", "F", "G", "H", "Metadata", "Bad", "Reused", "Carried", "Partial"]:
            (fixture / f"release-{label}").touch()
        for process in processes:
            if process.poll() is None:
                try:
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    process.terminate()
                    process.wait(timeout=10)
    verify_external_index(fixture, sources["A"], isolated)
    verify_service_order(fixture, run, isolated)


def verify_external_index(fixture, source, environment):
    """Adopt immutable old evidence and refuse corrupt copies through real CLI calls."""
    home = fixture / "index-home"
    home.mkdir()
    env = {**environment, "HOME": str(home), "USERPROFILE": str(home)}
    original = fixture / "A"
    receipt = json.loads((original / "receipt.json").read_text(encoding="utf-8"))
    legacy = fixture / "legacy-external-已签名"
    shutil.copytree(original, legacy)
    original_bytes = {path.relative_to(legacy).as_posix(): hashlib.sha256(path.read_bytes()).hexdigest()
                      for path in legacy.rglob("*") if path.is_file()}

    def invoke(label, *arguments, success=True, settings=env):
        result = subprocess.run([sys.executable, str(source / ".github/scripts/local_acceptance.py"),
                                 *map(str, arguments)], cwd=source, env=settings, capture_output=True, timeout=30)
        data = result.stdout + result.stderr
        (fixture / (label + ".log")).write_bytes(data)
        assert (result.returncode == 0) == success, data.decode(errors="replace")
        return data.decode("utf-8", errors="replace")

    invoke("index-legacy", "index-output", "--directory", legacy, "--policy-ref", "main")
    parent = home / ".nexus/local-acceptance/results" / receipt["head"]
    references = list(parent.glob("external-*.json"))
    assert len(references) == 1
    assert Path(json.loads(references[0].read_bytes())["directory"]) == legacy.resolve()
    assert {path.relative_to(legacy).as_posix(): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in legacy.rglob("*") if path.is_file()} == original_bytes
    # Bad locators cannot turn another output into this source's progress.
    (parent / "external-malformed.json").write_text("[]", encoding="utf-8")
    (parent / "external-wrong-head.json").write_text(json.dumps({
        "version": 1, "head": "0" * 40, "directory": str(legacy),
    }), encoding="utf-8")
    events = fixture / "events.log"
    before = events.read_bytes()
    (fixture / "release-RecoveredExternal").touch()
    recovered = fixture / "RecoveredExternal"
    invoke("recover-indexed-ancestor", "check", "--base", "main", "--policy-ref", "main", "--output", recovered)
    carried = json.loads((recovered / "receipt.json").read_bytes())
    assert carried["carried"][0]["head"] == receipt["head"]
    assert carried["carried"][0]["profiles"] == ["native-workspace"]
    assert events.read_bytes() == before
    indexed_before = set(parent.glob("external-*.json"))
    for label in ["bad-log", "bad-signature"]:
        damaged = fixture / label
        shutil.copytree(original, damaged)
        if label == "bad-log":
            (damaged / "native-workspace/000.log").write_text("tampered\n", encoding="utf-8")
        else:
            envelope = json.loads((damaged / "envelope.json").read_bytes())
            envelope["signature"] = "YmFkIHNpZ25hdHVyZQ=="
            (damaged / "envelope.json").write_text(json.dumps(envelope), encoding="utf-8")
        text = invoke(label, "index-output", "--directory", damaged, "--policy-ref", "main", success=False)
        assert ("Indexed log missing or changed" if label == "bad-log" else "Invalid SSH acceptance signature") in text, text
        assert set(parent.glob("external-*.json")) == indexed_before
    # The index is a cache: a real I/O failure cannot invalidate signed results.
    blocked_home = fixture / "index-unavailable-home"
    blocked = blocked_home / ".nexus/local-acceptance/results" / carried["head"]
    blocked.parent.mkdir(parents=True)
    blocked.write_text("controlled index collision\n", encoding="utf-8")
    blocked_env = {**environment, "HOME": str(blocked_home), "USERPROFILE": str(blocked_home)}
    retained = fixture / "IndexUnavailable"
    text = invoke("index-io-failure-preserves-proof", "check", "--base", "main", "--policy-ref", "main",
                  "--resume-from", recovered, "--output", retained, settings=blocked_env)
    assert "LOCAL_ACCEPTANCE_OUTPUT_INDEX_UNAVAILABLE" in text, text
    assert (retained / "envelope.json").is_file()
    assert events.read_bytes() == before
    invoke("explicit-index-io-failure-refused", "index-output", "--directory", retained,
           "--policy-ref", "main", settings=blocked_env, success=False)
    print("PASS: original external proof adoption, Unicode paths, wrong locators and corrupt log/signature refusal; index I/O failure retains verified progress")


def verify_service_order(fixture, run, environment):
    """Run real failing/successful profile children and verify the signed plan."""
    source = fixture / "service-order-source"
    run("git", "worktree", "add", "--detach", source, "main")
    profiles = source / ".github/local-acceptance/checks.py"
    profiles.write_text(
        'import sys\n'
        'def run(name, root, output, base, execute):\n'
        '    code = ("from pathlib import Path; import sys; "\n'
        '            "Path(sys.argv[1]).open(\'a\').write(sys.argv[2] + chr(10)); "\n'
        '            "failed = sys.argv[2] == \'native-services\' and Path(\'input.txt\').read_text().startswith(\'fail\'); "\n'
        '            "print(\'CONTROLLED_SERVICE_FAILURE\' if failed else \'passed:\' + sys.argv[2]); "\n'
        '            "raise SystemExit(23 if failed else 0)")\n'
        '    return [execute([sys.executable, "-c", code, str(root.parent / "service-order-events.log"), name], '
        'root, output / "000.log")]\n', encoding="utf-8")
    declared = ["preflight", "native-workspace", "rendering", "browser", "native-release", "native-services"]
    policy_path = source / "spec/local_acceptance.json"
    policy = json.loads(policy_path.read_text(encoding="utf-8"))
    policy["checks"] = {name: {"include": ["input.txt"]} for name in declared}
    policy_path.write_text(json.dumps(policy), encoding="utf-8")
    run("git", "add", ".", cwd=source)
    run("git", "commit", "-m", "Declare slow profiles before service acceptance", cwd=source)
    run("git", "branch", "service-order-policy", cwd=source)
    events = fixture / "service-order-events.log"

    def check(label, contents, *, success, resume=None):
        if contents is not None:
            (source / "input.txt").write_text(contents, encoding="utf-8")
            run("git", "commit", "-am", label, cwd=source)
        output = fixture / label
        arguments = [sys.executable, str(source / ".github/scripts/local_acceptance.py"), "check",
                     "--base", "service-order-policy", "--policy-ref", "service-order-policy", "--output", str(output)]
        if resume is not None:
            arguments += ["--resume-from", str(resume)]
        result = subprocess.run(arguments, cwd=source, env=environment, capture_output=True)
        output.mkdir(exist_ok=True)
        (output / "process.log").write_bytes(result.stdout + result.stderr)
        assert (result.returncode == 0) == success, (result.stdout + result.stderr).decode(errors="replace")
        return output

    failed = check("service-order-failed", "fail service\n", success=False)
    observed = events.read_text(encoding="utf-8").splitlines()
    assert observed == ["preflight", "native-workspace", "native-services"], observed
    assert "CONTROLLED_SERVICE_FAILURE" in (failed / "process.log").read_text(encoding="utf-8")
    assert not (failed / "envelope.json").exists()
    assert json.loads((failed / "attempt.json").read_text(encoding="utf-8"))["outcome"] == "failure"
    events.unlink()
    passed = check("service-order-passed", "pass service\n", success=True)
    observed = events.read_text(encoding="utf-8").splitlines()
    assert observed == ["preflight", "native-workspace", "native-services", "rendering", "browser", "native-release"], observed
    receipt = json.loads((passed / "receipt.json").read_text(encoding="utf-8"))
    assert receipt["plan"]["local_checks"] == declared, receipt["plan"]
    assert set(receipt["checks"]) == set(declared)
    assert all(len(commands) == 1 and commands[0]["exit_code"] == 0 for commands in receipt["checks"].values())
    restored = check("service-order-restored", None, success=True, resume=passed)
    assert events.read_text(encoding="utf-8").splitlines() == observed
    assert (restored / "envelope.json").is_file()
    print("PASS: actual failed service blocks release/rendering/browser entry; success and verified resume retain every profile and the declared signed plan")
