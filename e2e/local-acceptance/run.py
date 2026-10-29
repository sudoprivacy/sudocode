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


def fixture_environment():
    """Clear hook-exported repository selectors before touching temporary repos."""
    names = subprocess.check_output(["git", "rev-parse", "--local-env-vars"],
                                    cwd=SOURCE, text=True).splitlines()
    return {key: value for key, value in os.environ.items() if key not in names
            and not key.startswith(("GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"))}


FIXTURE_ENV = fixture_environment()


def command(*args, cwd, success=True):
    result = subprocess.run(list(map(str, args)), cwd=cwd, capture_output=True,
                            text=True, encoding="utf-8", errors="replace", env=FIXTURE_ENV)
    if (result.returncode == 0) != success:
        raise RuntimeError(f"Unexpected exit {result.returncode}: {args}\n{result.stdout}\n{result.stderr}")
    return result.stdout.strip()


def scope_acceptance(directory):
    source = SOURCE / ".github/scripts/ci_input_scope.py"
    if not source.exists():
        return
    repository = directory / "scope"
    command("git", "init", "--initial-branch=main", repository, cwd=directory)
    assert Path(command("git", "rev-parse", "--show-toplevel", cwd=repository)).resolve() == repository.resolve()
    command("git", "config", "user.name", "Scope E2E", cwd=repository)
    command("git", "config", "user.email", "scope@example.invalid", cwd=repository)
    (repository / "README.md").write_text("base\n")
    command("git", "add", ".", cwd=repository)
    command("git", "commit", "-m", "base", cwd=repository)

    def expect_scope(event_name, event, live, daemon, clippy):
        data = directory / "scope-event.json"
        output = directory / "scope-output.txt"
        data.write_text(json.dumps(event))
        output.write_text("")
        environment = {**FIXTURE_ENV, "GITHUB_EVENT_PATH": str(data), "GITHUB_OUTPUT": str(output),
                       "GITHUB_EVENT_NAME": event_name}
        result = subprocess.run([sys.executable, str(source)], cwd=repository, env=environment, capture_output=True)
        assert result.returncode == 0, result.stderr.decode(errors="replace")
        assert f"live_required={str(live).lower()}" in output.read_text()
        assert f"daemon_required={str(daemon).lower()}" in output.read_text()
        assert f"clippy_required={str(clippy).lower()}" in output.read_text()

    def expect_native_targets(base, targets):
        selector = SOURCE / ".github/local-acceptance/checks.py"
        code = ('import importlib.util,json,sys; from pathlib import Path; '
                'spec=importlib.util.spec_from_file_location("checks",sys.argv[1]); '
                'module=importlib.util.module_from_spec(spec); spec.loader.exec_module(module); '
                'print(json.dumps(module.changed_cli_test_targets(Path(sys.argv[2]),sys.argv[3])))')
        actual = command(sys.executable, "-c", code, selector, repository, base, cwd=repository)
        assert json.loads(actual) == targets, (base, actual, targets)

    cases = [("docs/note.md", False, False, False), (".github/workflows/rust-ci.yml", False, False, False),
             ("rust/crates/api/src/lib.rs", True, True, True), ("e2e/nexus-a2a/run.sh", False, True, False),
             ("spec/cache_metrics.json", True, False, False), ("rust/crates/runtime/src/prompt.md", True, True, True),
             ("rust/crates/engine-acp/src/ui.html", True, True, True), ("rust/crates/runtime/README.md", False, False, False),
             ("rust/crates/rusty-sudocode-cli/tests/pty_file_operations.rs", True, False, True),
             ("rust/crates/rusty-sudocode-cli/tests/common/mod.rs", True, False, True),
             ("rust/crates/rusty-sudocode-cli/Cargo.toml", True, True, True),
             ("rust/crates/pty-harness/src/lib.rs", True, True, True),
             ("e2e/shared-services/check.py", False, True, False)]
    for index, (path, live, daemon, clippy) in enumerate(cases):
        base = command("git", "rev-parse", "HEAD", cwd=repository)
        file = repository / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text("new fixture input\n")
        command("git", "add", ".", cwd=repository)
        command("git", "commit", "-m", f"input {index}", cwd=repository)
        head = command("git", "rev-parse", "HEAD", cwd=repository)
        expect_native_targets(base, ["pty_file_operations"] if path.endswith("/tests/pty_file_operations.rs") else None)
        if path == "rust/crates/pty-harness/src/lib.rs":
            # The helper implementation may move out of tests/common. Its new
            # path must still select real native terminal acceptance.
            policy = SOURCE / "spec/local_acceptance.json"
            code = ('import importlib.util,json,subprocess,sys; '
                    'spec=importlib.util.spec_from_file_location("acceptance",sys.argv[1]); '
                    'module=importlib.util.module_from_spec(spec); spec.loader.exec_module(module); '
                    'paths=subprocess.check_output(["git","diff","--name-only","--no-renames",sys.argv[3],"HEAD"]).decode().splitlines(); '
                    'print(json.dumps(module.selected(json.load(open(sys.argv[2])),paths)))')
            selected = json.loads(command(sys.executable, "-c", code,
                SOURCE / ".github/scripts/local_acceptance.py", policy, base, cwd=repository))
            assert {"native-workspace", "native-resize", "rendering", "browser", "native-release"}.issubset(selected)
        for event_name, event in [("push", {"before": base, "after": head}),
                                  ("pull_request", {"pull_request": {"base": {"sha": base}, "head": {"sha": head}}}),
                                  ("merge_group", {"merge_group": {"base_sha": base, "head_sha": head}})]:
            expect_scope(event_name, event, live, daemon, clippy)
    # A rename across the test/production boundary must see both paths.
    base = command("git", "rev-parse", "HEAD", cwd=repository)
    command("git", "mv", "rust/crates/rusty-sudocode-cli/tests/pty_file_operations.rs",
            "rust/crates/runtime/src/moved.rs", cwd=repository)
    command("git", "commit", "-m", "move test source into production", cwd=repository)
    head = command("git", "rev-parse", "HEAD", cwd=repository)
    expect_native_targets(base, None)
    expect_scope("merge_group", {"merge_group": {"base_sha": base, "head_sha": head}}, True, True, True)
    expect_scope("workflow_dispatch", {}, True, True, True)
    base = head
    for name in ["pty_first", "pty_second"]:
        (repository / f"rust/crates/rusty-sudocode-cli/tests/{name}.rs").write_text("new fixture input\n")
    (repository / "README.md").write_text("documentation alongside two changed targets\n")
    command("git", "add", ".", cwd=repository)
    command("git", "commit", "-m", "two integration targets and docs", cwd=repository)
    expect_native_targets(base, ["pty_first", "pty_second"])
    base = command("git", "rev-parse", "HEAD", cwd=repository)
    (repository / "spec/cache_metrics.json").write_text("shared input changed\n")
    (repository / "rust/crates/rusty-sudocode-cli/tests/pty_second.rs").write_text("changed target and shared fixture\n")
    command("git", "add", ".", cwd=repository)
    command("git", "commit", "-m", "shared fixture keeps full acceptance", cwd=repository)
    expect_native_targets(base, None)
    base = command("git", "rev-parse", "HEAD", cwd=repository)
    command("git", "rm", "rust/crates/rusty-sudocode-cli/tests/pty_first.rs", cwd=repository)
    command("git", "commit", "-m", "deleted target keeps full acceptance", cwd=repository)
    expect_native_targets(base, None)
    print("PASS: real Git scopes for PR/push/queue, native test targets, shared fixtures, source rename and explicit dispatch")


def enforcement_acceptance():
    with tempfile.TemporaryDirectory(prefix="scode-local-guard-") as temporary:
        directory = Path(temporary)
        scope_acceptance(directory)
        print(command(sys.executable, str(Path(__file__).with_name("cache_acceptance.py")), cwd=SOURCE))
        origin, work = directory / "origin.git", directory / "author"
        command("git", "init", "--bare", str(origin), cwd=directory)
        command("git", "init", "--initial-branch=main", str(work), cwd=directory)
        assert Path(command("git", "rev-parse", "--show-toplevel", cwd=work)).resolve() == work.resolve()
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


def main():
    if sys.argv[1:] == ["--fixture-child"]:
        enforcement_acceptance()
        return
    with tempfile.TemporaryDirectory(prefix="scode-hook-parent-") as temporary:
        parent = Path(temporary)
        command("git", "init", "--initial-branch=main", parent, cwd=parent)
        command("git", "config", "user.name", "Hook parent", cwd=parent)
        command("git", "config", "user.email", "parent@example.invalid", cwd=parent)
        (parent / "sentinel.txt").write_text("The fixture must not modify its caller.\n")
        command("git", "add", ".", cwd=parent)
        command("git", "commit", "-m", "Preserve hook caller", cwd=parent)
        before = {name: (parent / ".git" / name).read_bytes() for name in ("HEAD", "index", "config")}
        head = command("git", "rev-parse", "HEAD", cwd=parent)
        environment = {**FIXTURE_ENV, "GIT_DIR": str(parent / ".git"),
                       "GIT_WORK_TREE": str(parent), "GIT_INDEX_FILE": str(parent / ".git/index"),
                       "GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "user.name",
                       "GIT_CONFIG_VALUE_0": "Inherited hook identity"}
        result = subprocess.run([sys.executable, str(Path(__file__).resolve()), "--fixture-child"],
                                cwd=parent, env=environment)
        assert result.returncode == 0, "Acceptance failed with a real inherited hook environment"
        assert all((parent / ".git" / name).read_bytes() == data for name, data in before.items())
        assert command("git", "rev-parse", "HEAD", cwd=parent) == head
        assert not command("git", "status", "--porcelain", cwd=parent)
        print("PASS: inherited hook environment leaves caller HEAD, index, config and source unchanged")


if __name__ == "__main__":
    main()
