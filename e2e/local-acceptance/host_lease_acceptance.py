"""Exercise the host acceptance lease through concurrent CLI processes, Git and SSH."""

import ast
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
    verify_service_order(fixture, source, run, isolated)
    verify_lint_preparation_order(fixture, run, isolated)
    verify_profile_handoff(fixture, source, run, isolated)
    verify_static_before_queue(fixture, source, run, isolated)


def clone_private_policy(fixture, authority, run, directory):
    """Give each cloned policy its own identity and the original fixture key."""
    repository = directory / "policy"
    run("git", "clone", "--no-hardlinks", "--branch", "main", fixture / "work", repository, cwd=directory)
    run("git", "config", "user.name", "Host Lease E2E", cwd=repository)
    run("git", "config", "user.email", "host-lease@example.invalid", cwd=repository)
    shutil.copyfile(authority / ".github/scripts/local_acceptance.py",
                    repository / ".github/scripts/local_acceptance.py")
    run("git", "config", "localAcceptance.signer", "fixture", cwd=repository)
    run("git", "config", "localAcceptance.key", fixture / "device-key", cwd=repository)
    return repository


def verify_profile_handoff(fixture, authority, run, environment):
    """Let an existing waiter run between profiles and verify cumulative wait."""
    directory = fixture / "profile-handoff"
    directory.mkdir()
    repository = clone_private_policy(fixture, authority, run, directory)
    policy = repository / "spec/local_acceptance.json"
    rules = json.loads(policy.read_text())
    rules["checks"]["native-services"] = {"include": ["second.txt"]}
    policy.write_text(json.dumps(rules))
    (repository / "phase.py").write_text(
        'from pathlib import Path\nimport sys,time\n'
        'events,release,label=map(str,sys.argv[1:])\n'
        'with Path(events).open("a") as stream: stream.write("start:"+label+"\\n")\n'
        'deadline=time.monotonic()+120\n'
        'while not Path(release).exists() and time.monotonic()<deadline: time.sleep(0.05)\n'
        'assert Path(release).exists(), "release never arrived"\n'
        'with Path(events).open("a") as stream: stream.write("end:"+label+"\\n")\n', encoding="utf-8")
    (repository / ".github/local-acceptance/checks.py").write_text(
        'import sys\n'
        'def run(name, root, output, base, execute):\n'
        '    if name == "preflight":\n'
        '        return [execute([sys.executable,"-c","print(\'metadata passed\')"],root,output/"000.log")]\n'
        '    label=output.parent.name+("-two" if name=="native-services" else "")\n'
        '    return [execute([sys.executable,str(root/"phase.py"),str(root.parent/"events.log"),'
        'str(root.parent/("release-"+label)),label],root,output/"000.log")]\n', encoding="utf-8")
    run("git", "add", ".", cwd=repository)
    run("git", "commit", "-m", "Protected two-profile FIFO fixture", cwd=repository)
    home = directory / "home"
    home.mkdir()
    env = {key: value for key, value in environment.items() if key != LEASE_ENV}
    env.update(HOME=str(home), USERPROFILE=str(home))
    processes = []

    def events():
        path = directory / "events.log"
        return path.read_text().splitlines() if path.exists() else []

    def wait_until(condition, description):
        deadline = time.monotonic() + 120
        while not condition():
            assert time.monotonic() < deadline, f"Timed out waiting for {description}: {events()}"
            time.sleep(0.05)

    def start(label, second=False):
        work = directory / ("source-" + label)
        run("git", "worktree", "add", "--detach", work, "main", cwd=repository)
        (work / "input.txt").write_text(label)
        if second:
            (work / "second.txt").write_text("second required profile")
        run("git", "add", ".", cwd=work)
        run("git", "commit", "-m", "Candidate " + label, cwd=work)
        with (directory / (label + ".log")).open("wb") as stream:
            process = subprocess.Popen([sys.executable, str(work / ".github/scripts/local_acceptance.py"),
                "check", "--base", "main", "--policy-ref", "main", "--output", str(directory / label)],
                cwd=work, env=env, stdout=stream, stderr=subprocess.STDOUT)
        processes.append(process)
        return process

    def queued(process):
        return bool(list((home / ".nexus/local-acceptance/queue").glob(f"*-{process.pid}.ticket")))

    try:
        holder = start("Holder")
        wait_until(lambda: events() == ["start:Holder"], "the private holder")
        long = start("Long", second=True)
        wait_until(lambda: queued(long), "the two-profile contender to queue")
        short = start("Short")
        wait_until(lambda: queued(short), "the existing one-profile contender to queue")
        (directory / "release-Holder").touch()
        wait_until(lambda: "start:Long" in events(), "the first long profile")
        (directory / "release-Long").touch()
        wait_until(lambda: "start:Short" in events() or "start:Long-two" in events(), "the next actual profile")
        assert "start:Short" in events() and "start:Long-two" not in events(), events()
        wait_until(lambda: queued(long), "the next long profile to rejoin the FIFO")
        (directory / "release-Short").touch()
        wait_until(lambda: "start:Long-two" in events(), "the second long profile after the existing waiter")
        (directory / "release-Long-two").touch()
        for process in processes:
            assert process.wait(timeout=120) == 0
        assert events() == ["start:Holder", "end:Holder", "start:Long", "end:Long",
                            "start:Short", "end:Short", "start:Long-two", "end:Long-two"], events()
        receipt = json.loads((directory / "Long/receipt.json").read_text())
        assert set(receipt["checks"]) == {"preflight", "native-workspace", "native-services"}, receipt
        assert (directory / "Long/envelope.json").is_file()
        attempt = json.loads((directory / "Long/attempt.json").read_text())
        acquisitions = attempt["host_lease_acquisitions"]
        assert [entry["profile"] for entry in acquisitions] == ["native-workspace", "native-services"], attempt
        assert all(entry["wait_seconds"] > 0 for entry in acquisitions), attempt
        assert abs(attempt["host_lease_wait_seconds"] - sum(entry["wait_seconds"] for entry in acquisitions)) < 0.002, attempt
        assert not list((home / ".nexus/local-acceptance/queue").glob("*.ticket"))
        print("PASS: existing contender completes between two exclusive required profiles; all signed profiles finish once and cumulative FIFO wait is retained")
    finally:
        for label in ["Holder", "Long", "Short", "Long-two"]:
            (directory / ("release-" + label)).touch()
        for process in processes:
            if process.poll() is None:
                try:
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    process.terminate()
                    process.wait(timeout=10)


def verify_static_before_queue(fixture, authority, run, environment):
    """Refuse a real production pin mismatch while another private profile holds the host."""
    directory = fixture / "static-before-queue"
    directory.mkdir()
    repository = clone_private_policy(fixture, authority, run, directory)
    holder_policy = run("git", "rev-parse", "HEAD", cwd=repository)
    home = directory / "home"
    home.mkdir()
    env = {key: value for key, value in environment.items() if key != LEASE_ENV}
    env.update(HOME=str(home), USERPROFILE=str(home))
    processes = []

    def start(label, policy):
        work = directory / ("source-" + label)
        run("git", "worktree", "add", "--detach", work, policy, cwd=repository)
        (work / "input.txt").write_text(label)
        run("git", "add", ".", cwd=work)
        run("git", "commit", "-m", "Candidate " + label, cwd=work)
        with (directory / (label + ".log")).open("wb") as stream:
            process = subprocess.Popen([sys.executable, str(work / ".github/scripts/local_acceptance.py"),
                "check", "--base", policy, "--policy-ref", policy, "--output", str(directory / label)],
                cwd=work, env=env, stdout=stream, stderr=subprocess.STDOUT)
        processes.append(process)
        return process

    # Keep the actual production validator and staging code. Only preflight's
    # executable is private, so this control never starts Docker or native tests.
    source = Path(__file__).resolve().parents[2]
    contents = (source / ".github/local-acceptance/checks.py").read_text(encoding="utf-8")
    lines = contents.splitlines(keepends=True)
    definition = next(node for node in ast.parse(contents).body
                      if isinstance(node, ast.FunctionDef) and node.name == "run")
    lines[definition.lineno - 1] = lines[definition.lineno - 1].replace("def run(", "def production_run(", 1)
    contents = "".join(lines) + (
        '\ndef run(name, root, output, base, execute):\n'
        '    if name == "preflight":\n'
        '        return [execute([sys.executable,"-c","print(\'metadata passed\')"],root,output/"000.log")]\n'
        '    return production_run(name, root, output, base, execute)\n')
    try:
        holder = start("Holder", holder_policy)
        events = directory / "events.log"
        deadline = time.monotonic() + 120
        while not events.exists() or events.read_text().splitlines() != ["start:Holder"]:
            assert time.monotonic() < deadline, "Private holder never entered its profile"
            time.sleep(0.05)
        (repository / ".github/local-acceptance/checks.py").write_text(contents, encoding="utf-8")
        # This is the first input staged by native_guard_files. Its real pin
        # remains unchanged; a committed mismatch must fail before host admission.
        (repository / ".github/scripts/check_pr_history.py").write_text("raise SystemExit('DO_NOT_EXECUTE')\n")
        policy = repository / "spec/local_acceptance.json"
        rules = json.loads(policy.read_text())
        rules["checks"] = {"preflight": {"include": ["*"]}, "guard-contract": {"include": ["input.txt"]}}
        policy.write_text(json.dumps(rules))
        run("git", "add", ".", cwd=repository)
        run("git", "commit", "-m", "Protected invalid guard pin fixture", cwd=repository)
        bad_policy = run("git", "rev-parse", "HEAD", cwd=repository)
        bad = start("Bad", bad_policy)
        assert bad.wait(timeout=30) == 1
        log = (directory / "Bad.log").read_text(encoding="utf-8")
        assert "GUARD_STATIC_PREFLIGHT:" in log, log
        assert "Native guard input .github/scripts/check_pr_history.py changed: SHA256" in log, log
        assert "Update its protected pin" in log, log
        assert "Waiting for this host" not in log and "Acquired this host" not in log, log
        assert "DOCKER_ENGINE_PROBE" not in log, log
        assert holder.poll() is None and events.read_text().splitlines() == ["start:Holder"]
        attempt = json.loads((directory / "Bad/attempt.json").read_text())
        assert attempt["outcome"] == "failure" and attempt["host_lease_wait_seconds"] == 0, attempt
        assert attempt["host_lease_acquisitions"] == [], attempt
        assert not (directory / "Bad/envelope.json").exists()
        checkpoint = sorted((directory / "Bad/checkpoints").iterdir())[-1]
        assert set(json.loads((checkpoint / "receipt.json").read_text())["checks"]) == {"preflight"}
        assert not list((home / ".nexus/local-acceptance/queue").glob("*.ticket"))
        (directory / "release-Holder").touch()
        assert holder.wait(timeout=30) == 0
        print("PASS: actual protected pin mismatch fails before a held host queue, without Docker/native entry; signed preflight progress remains incomplete")
    finally:
        # Terminate only this private refused contender before releasing its
        # holder, including when the unchanged-helper counterexample times out.
        for process in processes[1:]:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=10)
        (directory / "release-Holder").touch()
        for process in processes[:1]:
            if process.poll() is None:
                try:
                    process.wait(timeout=15)
                except subprocess.TimeoutExpired:
                    process.terminate()
                    process.wait(timeout=10)


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


def verify_lint_preparation_order(fixture, run, environment):
    """Drive the real signed CLI with child failures before and after preparation."""
    source = fixture / "lint-order-source"
    run("git", "worktree", "add", "--detach", source, "main")
    profiles = source / ".github/local-acceptance/checks.py"
    profiles.write_text('''import sys
def emit(name, root, output, execute):
    output.parent.mkdir(parents=True, exist_ok=True)
    code = ("from pathlib import Path; import sys; "
            "root = Path(sys.argv[1]); name = sys.argv[2]; "
            "(root.parent / 'lint-order-events.log').open('a').write(name + chr(10)); "
            "failed = (name == 'lint' and (root / 'input.txt').read_text().startswith('fail')) or "
            "(name == 'guard-contract' and (root.parent / 'lint-order-refuse-guard').exists()); "
            "print('CONTROLLED_FAILURE:' + name if failed else 'passed:' + name); "
            "raise SystemExit(23 if failed else 0)")
    return execute([sys.executable, '-c', code, str(root), name], root, output)
def prepare_native_workspace_lint(root, output, base, execute):
    return [emit('lint', root, output / 'native-workspace' / '000.log', execute)]
def run(name, root, output, base, execute):
    if name == 'native-workspace':
        original = run.prepared_workspace
        del run.prepared_workspace
        return [*original, emit('tests', root, output / '001.log', execute)]
    return [emit(name, root, output / '000.log', execute)]
''', encoding="utf-8")
    declared = ["preflight", "guard-contract", "native-workspace"]
    policy_path = source / "spec/local_acceptance.json"
    policy = json.loads(policy_path.read_text(encoding="utf-8"))
    policy["checks"] = {name: {"include": ["input.txt"]} for name in declared}
    policy_path.write_text(json.dumps(policy), encoding="utf-8")
    run("git", "add", ".", cwd=source)
    run("git", "commit", "-m", "Declare early lint fixture", cwd=source)
    run("git", "branch", "lint-order-policy", cwd=source)
    events = fixture / "lint-order-events.log"

    def check(label, contents, *, success, resume=None):
        if contents is not None:
            (source / "input.txt").write_text(contents, encoding="utf-8")
            run("git", "commit", "-am", label, cwd=source)
        output = fixture / label
        arguments = [sys.executable, str(source / ".github/scripts/local_acceptance.py"), "check",
                     "--base", "lint-order-policy", "--policy-ref", "lint-order-policy", "--output", str(output)]
        if resume is not None:
            arguments += ["--resume-from", str(resume)]
        process = subprocess.run(arguments, cwd=source, env=environment, capture_output=True)
        output.mkdir(exist_ok=True)
        (output / "process.log").write_bytes(process.stdout + process.stderr)
        assert (process.returncode == 0) == success, (process.stdout + process.stderr).decode(errors="replace")
        return output

    refused = check("lint-order-refused", "fail lint\n", success=False)
    assert events.read_text().splitlines() == ["preflight", "lint"]
    assert "CONTROLLED_FAILURE:lint" in (refused / "process.log").read_text()
    assert not (refused / "envelope.json").exists()
    def completed(output):
        pointer = json.loads((output / "checkpoint.json").read_text())
        receipt = output / "checkpoints" / pointer["generation"] / "receipt.json"
        return set(json.loads(receipt.read_text())["checks"])

    assert completed(refused) == {"preflight"}
    events.unlink()
    guard = fixture / "lint-order-refuse-guard"
    guard.touch()
    incomplete = check("lint-order-incomplete", "pass lint\n", success=False)
    assert events.read_text().splitlines() == ["preflight", "lint", "guard-contract"]
    assert not (incomplete / "envelope.json").exists()
    assert completed(incomplete) == {"preflight"}
    guard.unlink()
    events.unlink()
    passed = check("lint-order-resumed", None, success=True, resume=incomplete)
    assert events.read_text().splitlines() == ["lint", "guard-contract", "tests"]
    receipt = json.loads((passed / "receipt.json").read_text())
    assert receipt["plan"]["local_checks"] == declared, receipt["plan"]
    assert set(receipt["checks"]) == set(declared)
    assert len(receipt["checks"]["native-workspace"]) == 2
    assert all(command["exit_code"] == 0 for commands in receipt["checks"].values() for command in commands)
    assert len(json.loads((passed / "attempt.json").read_text())["host_lease_acquisitions"]) == 3
    before = events.read_bytes()
    restored = check("lint-order-restored", None, success=True, resume=passed)
    assert events.read_bytes() == before, "Complete signed resume repeated early lint or workspace tests"
    assert (restored / "envelope.json").is_file()
    print("PASS: real signed CLI refuses lint before guard, signs no prepared workspace after guard failure, "
          "rejoins the host FIFO, repeats unsigned preparation on retry and consumes passing commands exactly once")


def verify_service_order(fixture, authority, run, environment):
    """Run real failing/successful profile children and verify the signed plan."""
    source = fixture / "service-order-source"
    run("git", "worktree", "add", "--detach", source, "main")
    profiles = source / ".github/local-acceptance/checks.py"
    shutil.copyfile(authority / ".github/local-acceptance/cross_platform.py",
                    profiles.with_name("cross_platform.py"))
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

    # Execute the production CI entrypoint in its trusted private tool snapshot.
    # Only tiny profile inputs differ; real children and error propagation run.
    native = ["native-workspace", "native-release", "native-services"]
    ci_environment = {key: value for key, value in environment.items()
                      if not key.startswith("GITHUB_")}
    for label, contents, success, expected in [
        ("ci-service-failed", "fail service\n", False, ["native-workspace", "native-services"]),
        ("ci-service-passed", "pass service\n", True,
         ["native-workspace", "native-services", "native-release"]),
        ("ci-release-only", "pass service\n", True, ["native-release"]),
    ]:
        events.unlink()
        (source / "input.txt").write_text(contents, encoding="utf-8")
        selected = ["native-release"] if label == "ci-release-only" else native
        output = fixture / label
        result = subprocess.run([sys.executable, str(profiles.with_name("cross_platform.py"))],
            cwd=source, env={**ci_environment, "ACCEPTANCE_NATIVE_CHECKS": " ".join(selected),
                            "ACCEPTANCE_OUTPUT": str(output), "ACCEPTANCE_BASE": "service-order-policy"},
            capture_output=True)
        output.mkdir(exist_ok=True)
        log = (result.stdout + result.stderr).decode("utf-8", errors="replace")
        (output / "process.log").write_text(log, encoding="utf-8")
        assert (result.returncode == 0) == success, log
        assert events.read_text(encoding="utf-8").splitlines() == expected, log
        report = json.loads(next(line.split("=", 1)[1] for line in log.splitlines()
                                 if line.startswith("CI_NATIVE_EXECUTION_ORDER=")))
        assert report["declared_checks"] == selected, report
        assert report["execution_order"] == (["native-workspace", "native-services", "native-release"]
                                              if selected == native else selected), report
        if not success:
            assert "CONTROLLED_SERVICE_FAILURE" in log, log
            assert not (output / "native-release").exists()
    (source / "input.txt").write_text("pass service\n", encoding="utf-8")
    print("PASS: real complementary CI entrypoint shares service-first order, fails before release and preserves release-only selection")
