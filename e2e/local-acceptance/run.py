#!/usr/bin/env python3
"""Exercise real Git pushes, automatic hooks, SSH signatures and notes remotes.

Small executable fixture commands test the enforcement path. They are never
reported as product PTY/browser/performance acceptance.
"""

import base64
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


SOURCE = Path(__file__).resolve().parents[2]


def command(*args, cwd, success=True):
    result = subprocess.run(list(map(str, args)), cwd=cwd, capture_output=True,
                            text=True, encoding="utf-8", errors="replace")
    if (result.returncode == 0) != success:
        raise RuntimeError(f"Unexpected exit {result.returncode}: {args}\n{result.stdout}\n{result.stderr}")
    return result.stdout.strip()


def scope_acceptance(directory):
    source = SOURCE / ".github/scripts/ci_input_scope.py"
    if not source.exists():
        return
    repository = directory / "scope"
    command("git", "init", "--initial-branch=main", repository, cwd=directory)
    command("git", "config", "user.name", "Scope E2E", cwd=repository)
    command("git", "config", "user.email", "scope@example.invalid", cwd=repository)
    (repository / "README.md").write_text("base\n")
    command("git", "add", ".", cwd=repository)
    command("git", "commit", "-m", "base", cwd=repository)
    cases = [("docs/note.md", False, False), (".github/workflows/rust-ci.yml", False, False),
             ("rust/crates/api/src/lib.rs", True, True), ("e2e/nexus-a2a/run.sh", False, True),
             ("spec/cache_metrics.json", True, False), ("rust/crates/runtime/src/prompt.md", True, True),
             ("rust/crates/engine-acp/src/ui.html", True, True), ("rust/crates/runtime/README.md", False, False)]
    for index, (path, live, daemon) in enumerate(cases):
        base = command("git", "rev-parse", "HEAD", cwd=repository)
        file = repository / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text("new fixture input\n")
        command("git", "add", ".", cwd=repository)
        command("git", "commit", "-m", f"input {index}", cwd=repository)
        head = command("git", "rev-parse", "HEAD", cwd=repository)
        for event_name, event in [("push", {"before": base, "after": head}),
                                  ("merge_group", {"merge_group": {"base_sha": base, "head_sha": head}})]:
            data = directory / "scope-event.json"
            output = directory / "scope-output.txt"
            data.write_text(json.dumps(event))
            output.write_text("")
            environment = {**os.environ, "GITHUB_EVENT_PATH": str(data), "GITHUB_OUTPUT": str(output),
                           "GITHUB_EVENT_NAME": event_name}
            result = subprocess.run([sys.executable, str(source)], cwd=repository, env=environment, capture_output=True)
            assert result.returncode == 0, result.stderr.decode(errors="replace")
            assert f"live_required={str(live).lower()}" in output.read_text()
            assert f"daemon_required={str(daemon).lower()}" in output.read_text()
    print("PASS: real Git input scopes for docs, CI, Rust, daemon and cache specs")


def main():
    with tempfile.TemporaryDirectory(prefix="scode-local-guard-") as temporary:
        directory = Path(temporary)
        scope_acceptance(directory)
        origin, work = directory / "origin.git", directory / "author"
        command("git", "init", "--bare", str(origin), cwd=directory)
        command("git", "init", "--initial-branch=main", str(work), cwd=directory)
        command("git", "config", "user.name", "Acceptance E2E", cwd=work)
        command("git", "config", "user.email", "acceptance@example.invalid", cwd=work)
        command("git", "remote", "add", "origin", str(origin), cwd=work)
        helper = work / ".github/scripts/local_acceptance.py"
        helper.parent.mkdir(parents=True)
        shutil.copyfile(SOURCE / ".github/scripts/local_acceptance.py", helper)
        (work / ".github/local-acceptance").mkdir()
        (work / ".github/local-acceptance/Dockerfile").write_text("FROM scratch\n")
        (work / ".github/workflows").mkdir()
        (work / ".github/workflows/local-acceptance.yml").write_text("# Fixture metadata only\n")
        (work / ".github/workflows/local-acceptance-publish.yml").write_text("# Fixture trusted publisher\n")
        (work / ".github/local-acceptance/checks.py").write_text(
            'import sys\n'
            'def run(name, root, output, base, execute):\n'
            '    return [execute([sys.executable, "-c", '
            '"from pathlib import Path; text=Path(\'input.txt\').read_text(); print(text); raise SystemExit(text != \'pass\')"], '
            'root, output / "actual-process.log")]\n'
        )
        (work / "spec").mkdir()
        (work / "spec/local_acceptance.json").write_text(json.dumps({
            "version": 1, "repository": "acceptance/fixture", "checks": {"real-process": {"include": ["**"]}}
        }))
        key = directory / "device-key"
        command("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key), cwd=work)
        system = {"win32": "windows", "darwin": "macos"}.get(sys.platform, "linux")
        (work / "spec/local_acceptance_signers.json").write_text(json.dumps({"version": 1, "signers": {
            "fixture": {"platform": system, "public_key": key.with_suffix(".pub").read_text().strip()}
        }}))
        (work / ".gitignore").write_text("__pycache__/\n")
        (work / "input.txt").write_text("pass")
        command("git", "add", ".", cwd=work)
        command("git", "commit", "-m", "Fixture base", cwd=work)
        command("git", "push", "origin", "main", cwd=work)
        command("git", "checkout", "-b", "feature", cwd=work)
        command(sys.executable, helper, "install", "--signer", "fixture", "--key", key, cwd=work)

        # Failed executable commands must block the actual Git transport.
        (work / "feature.txt").write_text("feature input\n")
        (work / "input.txt").write_text("fail")
        command("git", "add", ".", cwd=work)
        command("git", "commit", "-m", "Fail the real command", cwd=work)
        command("git", "push", "origin", "feature", cwd=work, success=False)
        assert not command("git", "ls-remote", "origin", "refs/heads/feature", cwd=work)
        command(sys.executable, helper, "verify", cwd=work, success=False)

        # Nothing is manually marked passed: pre-push runs, signs, publishes
        # notes, and only then allows the original branch push to complete.
        (work / "input.txt").write_text("pass")
        command("git", "add", ".", cwd=work)
        command("git", "commit", "-m", "Pass the real command", cwd=work)
        command("git", "push", "origin", "feature", cwd=work)
        head = command("git", "rev-parse", "HEAD", cwd=work)
        command("git", "fetch", "origin", "+refs/notes/local-acceptance:refs/notes/local-acceptance", cwd=work)
        command(sys.executable, helper, "verify", cwd=work)
        note = command("git", "notes", "--ref=local-acceptance", "show", head, cwd=work)
        envelope = json.loads(note)["attestations"]["fixture"]
        receipt = json.loads(base64.b64decode(envelope["payload"]))
        assert receipt["checks"]["real-process"][0]["exit_code"] == 0
        assert receipt["checks"]["real-process"][0]["elapsed_seconds"] > 0

        # A protected scheduling-only change must preserve a real completed
        # command's evidence. This is a local bare fixture, not a production
        # branch-protection bypass.
        command("git", "checkout", "main", cwd=work)
        workflow = work / ".github/workflows/local-acceptance.yml"
        workflow.write_text(workflow.read_text() + "# scheduling-only change\n")
        command("git", "add", ".", cwd=work)
        command("git", "commit", "-m", "Protected scheduler update", cwd=work)
        command("git", "fetch", str(work), "main:refs/heads/main", cwd=origin)
        command("git", "checkout", "feature", cwd=work)
        command("git", "fetch", "origin", "main", cwd=work)
        command(sys.executable, helper, "verify", cwd=work)
        linked = directory / "linked-author"
        command("git", "worktree", "add", "--detach", linked, head, cwd=work)
        installed = work / ".git/local-acceptance-hooks/local_acceptance.py"
        command(sys.executable, installed, "verify", cwd=linked)
        assert linked.resolve().parent == directory.resolve()
        command("git", "worktree", "remove", linked, cwd=work)

        # Both a new commit and unsigned changes to a previously valid receipt
        # must fail real SSH verification rather than borrowing an older pass.
        command("git", "commit", "--allow-empty", "-m", "New head needs a new receipt", cwd=work)
        command(sys.executable, helper, "verify", cwd=work, success=False)
        command("git", "push", "origin", "feature", cwd=work)
        new_head = command("git", "rev-parse", "HEAD", cwd=work)
        command("git", "fetch", "origin", "+refs/notes/local-acceptance:refs/notes/local-acceptance", cwd=work)
        command(sys.executable, helper, "verify", cwd=work)
        # Publishing the second commit preserved the first commit's note.
        assert command("git", "notes", "--ref=local-acceptance", "show", head, cwd=work) == note
        forged = json.loads(note)
        payload = json.loads(base64.b64decode(forged["attestations"]["fixture"]["payload"]))
        payload["head"] = new_head
        forged["attestations"]["fixture"]["payload"] = base64.b64encode(json.dumps(payload).encode()).decode()
        data = directory / "forged.json"
        data.write_text(json.dumps(forged))
        command("git", "notes", "--ref=local-acceptance", "add", "-f", "-F", data, new_head, cwd=work)
        command(sys.executable, helper, "verify", cwd=work, success=False)

        # A dirty source file cannot be silently included in tested inputs.
        (work / "input.txt").write_text("uncommitted")
        command(sys.executable, helper, "check", cwd=work, success=False)
        print("PASS: actual hook/push, failed process, signed notes, fresh head, tamper, preserved notes and dirty inputs")


if __name__ == "__main__":
    main()
