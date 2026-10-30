"""Exercise signed-profile resume through actual CLI processes, Git and SSH."""

import base64
import json
from pathlib import Path
import shutil
import sys
import tempfile


def verify_resume(directory, source, command):
    fixture = directory / "resume"
    fixture.mkdir()
    work, origin = fixture / "work", fixture / "origin.git"

    def run(*args, cwd=work, **kwargs):
        return command(*args, cwd=cwd, **kwargs)

    run("git", "init", "--bare", origin, cwd=fixture)
    run("git", "init", "--initial-branch=main", work, cwd=fixture)
    run("git", "config", "user.name", "Resume E2E")
    run("git", "config", "user.email", "resume@example.invalid")
    run("git", "remote", "add", "origin", origin)
    helper = work / ".github/scripts/local_acceptance.py"
    helper.parent.mkdir(parents=True)
    shutil.copyfile(source / ".github/scripts/local_acceptance.py", helper)
    profiles = work / ".github/local-acceptance/checks.py"
    profiles.parent.mkdir()
    profiles.write_text(
        'import sys\n'
        'def rendering_recipe():\n'
        '    return "render-v1"\n'
        'def step(root, output, execute, label):\n'
        '    code = "from pathlib import Path; import sys; '
        'Path(sys.argv[1]).open(\'a\').write(sys.argv[2]+chr(10)); print(sys.argv[2])"\n'
        '    return [execute([sys.executable, "-c", code, str(root.parent / "events.log"), label], '
        'root, output / "000.log")]\n'
        'def run(name, root, output, base, execute):\n'
        '    if name == "native-workspace":\n'
        '        return step(root, output, execute, "native-v1")\n'
        '    elif name == "rendering":\n'
        '        return step(root, output, execute, rendering_recipe())\n'
    )
    (profiles.parent / "Dockerfile").write_text("FROM scratch\n")
    workflows = work / ".github/workflows"
    workflows.mkdir()
    for name in ["local-acceptance.yml", "local-acceptance-publish.yml"]:
        (workflows / name).write_text("# Local enforcement fixture\n")
    (work / "spec").mkdir()
    (work / "spec/local_acceptance.json").write_text(json.dumps({
        "version": 1, "repository": "acceptance/resume-fixture", "checks": {
            "native-workspace": {"include": ["input.txt"]},
            "rendering": {"include": ["input.txt"]},
        },
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
    run("git", "commit", "-m", "Protected resume fixture")
    run("git", "push", "origin", "main")
    run("git", "checkout", "-b", "feature")
    (work / "input.txt").write_text("actual candidate\n")
    run("git", "add", ".")
    run("git", "commit", "-m", "Candidate input")
    head = run("git", "rev-parse", "HEAD")
    first, second, third = (fixture / name for name in ["first", "second", "third"])

    def check(output, resume=None, policy="main", **kwargs):
        args = [sys.executable, helper, "check", "--base", "main", "--policy-ref", policy,
                "--output", output, "--publish"]
        if resume is not None:
            args += ["--resume-from", resume]
        return run(*args, **kwargs)

    def events():
        return (fixture / "events.log").read_text().splitlines()

    check(first)
    assert events() == ["native-v1", "render-v1"]
    original = json.loads((first / "receipt.json").read_text())
    run("git", "checkout", "main")
    profiles.write_text(profiles.read_text().replace('return "render-v1"', 'return "render-v2"'))
    run("git", "add", ".")
    run("git", "commit", "-m", "Update only the protected rendering command")
    run("git", "push", "origin", "main")
    run("git", "checkout", "feature")
    assert run("git", "rev-parse", "HEAD") == head
    check(second, first)
    resumed = json.loads((second / "receipt.json").read_text())
    assert events() == ["native-v1", "render-v1", "render-v2"]
    assert resumed["resumed_from"]["profiles"] == ["native-workspace"]
    assert resumed["checks"]["native-workspace"] == original["checks"]["native-workspace"]
    assert (second / "native-workspace/000.log").read_bytes() == (first / "native-workspace/000.log").read_bytes()
    run("git", "fetch", "origin", "+refs/notes/local-acceptance:refs/notes/local-acceptance")
    run(sys.executable, helper, "verify", "--base", "main", "--policy-ref", "main")
    check(third, second)
    assert events() == ["native-v1", "render-v1", "render-v2"]
    assert json.loads((third / "receipt.json").read_text())["resumed_from"]["profiles"] == ["native-workspace", "rendering"]

    before_events = events()
    before_notes = run("git", "ls-remote", "origin", "refs/notes/local-acceptance")
    for label in ["payload", "signature", "log", "missing-log"]:
        damaged = fixture / ("damaged-" + label)
        shutil.copytree(first, damaged)
        if label == "payload":
            (damaged / "receipt.json").write_text("{}")
            error = "Resume receipt differs from its signed payload"
        elif label == "signature":
            envelope = json.loads((damaged / "envelope.json").read_text())
            envelope["signature"] = base64.b64encode(b"invalid signature").decode()
            (damaged / "envelope.json").write_text(json.dumps(envelope))
            error = "Invalid SSH acceptance signature"
        else:
            log = damaged / "native-workspace/000.log"
            if label == "log":
                log.write_text("damaged evidence\n")
            else:
                log.unlink()
            error = "Resume log missing or changed for native-workspace"
        check(fixture / ("rejected-" + label), damaged, success=False, error=error)
        assert events() == before_events
        assert run("git", "ls-remote", "origin", "refs/notes/local-acceptance") == before_notes

    # Even a valid SSH signature cannot transfer evidence to another architecture.
    different_host = fixture / "different-host"
    shutil.copytree(first, different_host)
    envelope = json.loads((different_host / "envelope.json").read_text())
    payload = json.loads(base64.b64decode(envelope["payload"]))
    payload["host"]["architecture"] = "different-architecture"
    payload["plan"]["author"]["architecture"] = "different-architecture"
    data = different_host / "receipt.json"
    data.write_text(json.dumps(payload))
    signature = data.with_suffix(".json.sig")
    signature.unlink()
    run("ssh-keygen", "-Y", "sign", "-f", key, "-n", "sudocode-local-acceptance", data)
    envelope.update(payload=base64.b64encode(data.read_bytes()).decode(),
                    signature=base64.b64encode(signature.read_bytes()).decode())
    (different_host / "envelope.json").write_text(json.dumps(envelope))
    check(fixture / "rejected-host", different_host, success=False,
          error="Resume receipt belongs to a different native host")
    assert events() == before_events

    # An unaccepted policy cannot lend valid signed evidence to protected main.
    unaccepted = run("git", "commit-tree", "main^{tree}", "-p", "main", "-m", "Unaccepted policy")
    run("git", "update-ref", "refs/heads/unaccepted-policy", unaccepted)
    untrusted = fixture / "unaccepted"
    check(untrusted, policy="unaccepted-policy")
    before_events = events()
    check(fixture / "rejected-policy", untrusted, success=False,
          error="Resume policy is not an ancestor of the current protected policy")
    assert events() == before_events
    run("git", "commit", "--allow-empty", "-m", "A new head still needs its own acceptance")
    check(fixture / "rejected-head", first, success=False,
          error="Acceptance is for another commit or tree")
    assert events() == before_events
    print("PASS: actual signed CLI resume reruns changed profiles; tampered/missing logs, signatures, host, head and unaccepted policy are refused")


if __name__ == "__main__":
    from run import SOURCE, command

    with tempfile.TemporaryDirectory(prefix="scode-resume-acceptance-") as temporary:
        verify_resume(Path(temporary), SOURCE, command)
