#!/usr/bin/env python3
"""Exercise real Git pushes, automatic hooks, SSH signatures and notes remotes.

Small executable fixture commands test the enforcement path. They are never
reported as product PTY/browser/performance acceptance.
"""

import argparse
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
    environment = {key: value for key, value in os.environ.items() if key not in names
                   and not key.startswith(("GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"))}
    environment["PYTHONIOENCODING"] = "utf-8"
    return environment


FIXTURE_ENV = fixture_environment()


def command(*args, cwd, success=True, error=None, stderr=False):
    result = subprocess.run(list(map(str, args)), cwd=cwd, capture_output=True,
                            text=True, encoding="utf-8", errors="replace", env=FIXTURE_ENV)
    if (result.returncode == 0) != success:
        raise RuntimeError(f"Unexpected exit {result.returncode}: {args}\n{result.stdout}\n{result.stderr}")
    if error is not None:
        assert error in result.stderr, result.stderr
    return (result.stderr if stderr else result.stdout).strip()


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

    def expect_scope(event_name, event, live, daemon, clippy, benchmark=False, container=False, clippy_executor=False,
                     gzip_codec=False, cohost=None, vfs_cache=False):
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
        assert f"cohost_required={str(daemon if cohost is None else cohost).lower()}" in output.read_text()
        assert f"clippy_required={str(clippy).lower()}" in output.read_text()
        assert f"benchmark_required={str(benchmark).lower()}" in output.read_text()
        assert f"container_required={str(container).lower()}" in output.read_text()
        assert f"clippy_executor_required={str(clippy_executor).lower()}" in output.read_text()
        assert f"gzip_codec_required={str(gzip_codec).lower()}" in output.read_text()
        assert f"vfs_cache_required={str(vfs_cache).lower()}" in output.read_text()

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
             ("spec/local_acceptance.json", False, False, False),
             ("spec/local_acceptance_signers.json", False, False, False),
             ("spec/cache_ttl_prices.json", True, False, False),
             ("rust/crates/engine-acp/src/ui.html", True, True, True), ("rust/crates/runtime/README.md", False, False, False),
             ("rust/crates/rusty-sudocode-cli/tests/pty_file_operations.rs", True, False, True),
             ("rust/crates/rusty-sudocode-cli/tests/common/mod.rs", True, False, True),
             ("rust/crates/rusty-sudocode-cli/Cargo.toml", True, True, True),
             ("rust/crates/rusty-sudocode-cli/src/repl_ui/tasks.rs", True, True, True),
             ("rust/crates/rusty-sudocode-cli/build.rs", True, True, True),
             ("rust/crates/pty-harness/src/lib.rs", True, True, True),
             ("e2e/shared-services/check.py", False, True, False),
             (".github/local-acceptance/checks.py", False, False, False),
             (".github/scripts/local_acceptance.py", False, False, False),
             ("e2e/local-acceptance/benchmark_targets_acceptance.py", False, False, False),
             ("e2e/terminal-resize/benchmark.py", False, False, False),
             ("e2e/terminal-resize/performance-policy.json", False, False, False),
             ("e2e/terminal-resize/gui-benchmark.py", False, False, False),
             ("e2e/terminal-resize/gui-policy.json", False, False, False),
             ("e2e/local-acceptance/container_inputs_acceptance.py", False, False, False),
             ("e2e/local-acceptance/clippy_fail_fast.py", False, False, False),
             ("rust/crates/runtime/src/response_archive.rs", True, True, True),
             ("rust/crates/runtime/src/response_archive/gzip.rs", True, True, True),
             ("e2e/response-gzip/main.rs", False, False, False),
             (".github/scripts/vfs_search_cache.py", False, True, False),
             (".github/actions/vfs-search-cache/action.yml", False, True, False),
             (".github/workflows/ci-vfs-search-cache.yml", False, True, False),
             ("e2e/local-acceptance/vfs_search_cache_acceptance.py", False, False, False)]
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
            assert {"native-workspace", "native-resize", "rendering", "browser"}.issubset(selected)
            assert "native-release" not in selected, "Dev-only PTY helper edits must not rebuild native releases"
        for event_name, event in [("push", {"before": base, "after": head}),
                                  ("pull_request", {"pull_request": {"base": {"sha": base}, "head": {"sha": head}}}),
                                  ("merge_group", {"merge_group": {"base_sha": base, "head_sha": head}})]:
            expect_scope(event_name, event, live, daemon, clippy, path in {
                ".github/workflows/rust-ci.yml", ".github/local-acceptance/checks.py",
                ".github/scripts/local_acceptance.py", "e2e/local-acceptance/benchmark_targets_acceptance.py",
                "e2e/terminal-resize/benchmark.py", "e2e/terminal-resize/performance-policy.json",
                "e2e/terminal-resize/gui-benchmark.py", "e2e/terminal-resize/gui-policy.json"}, path in {
                ".github/workflows/rust-ci.yml", ".github/local-acceptance/checks.py",
                ".github/scripts/local_acceptance.py", "e2e/local-acceptance/container_inputs_acceptance.py"}, path in {
                ".github/workflows/rust-ci.yml", "e2e/local-acceptance/clippy_fail_fast.py"}, path in {
                ".github/workflows/rust-ci.yml", ".github/local-acceptance/checks.py",
                ".github/scripts/local_acceptance.py", "rust/crates/runtime/src/response_archive.rs",
                "rust/crates/runtime/src/response_archive/gzip.rs", "e2e/response-gzip/main.rs"}, vfs_cache=path in {
                ".github/workflows/rust-ci.yml", ".github/scripts/vfs_search_cache.py",
                ".github/actions/vfs-search-cache/action.yml", ".github/workflows/ci-vfs-search-cache.yml",
                "e2e/local-acceptance/vfs_search_cache_acceptance.py"})
    # A rename across the test/production boundary must see both paths.
    base = command("git", "rev-parse", "HEAD", cwd=repository)
    command("git", "mv", "rust/crates/rusty-sudocode-cli/tests/pty_file_operations.rs",
            "rust/crates/runtime/src/moved.rs", cwd=repository)
    command("git", "commit", "-m", "move test source into production", cwd=repository)
    head = command("git", "rev-parse", "HEAD", cwd=repository)
    expect_native_targets(base, None)
    expect_scope("merge_group", {"merge_group": {"base_sha": base, "head_sha": head}}, True, True, True)
    expect_scope("workflow_dispatch", {}, True, True, True)
    expect_scope("push", {"before": "0" * 40, "after": head}, True, True, True)
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


def performance_scope_acceptance(directory):
    """Check committed changes through the public planner, including both rename paths."""
    repository = directory / "performance-scope"
    command("git", "init", "--initial-branch=main", repository, cwd=directory)
    command("git", "config", "user.name", "Performance scope E2E", cwd=repository)
    command("git", "config", "user.email", "scope@example.invalid", cwd=repository)
    for path in [".github/scripts/local_acceptance.py", "spec/local_acceptance.json"]:
        destination = repository / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(SOURCE / path, destination)
    command("git", "add", ".", cwd=repository)
    command("git", "commit", "-m", "Protected performance selection", cwd=repository)
    policy = command("git", "rev-parse", "HEAD", cwd=repository)
    helper = repository / ".github/scripts/local_acceptance.py"

    def expect(base, expected, *, native=True, guard=None, resize=None, release=None):
        text = command(sys.executable, helper, "plan", "--base", base, "--policy-ref", policy,
                       "--host", "windows", "--architecture", "AMD64", cwd=repository)
        plan = json.loads(next(line.split("=", 1)[1] for line in text.splitlines()
                               if line.startswith("ACCEPTANCE_PLAN=")))
        names = set(plan["local_checks"])
        assert "Hosted CI preflight also runs changed Cargo/Docker/Clippy executor fixtures" in text, text
        assert names & {"rendering", "browser"} == set(expected), text
        assert "Statistical performance profiles:" in text, text
        if not expected:
            assert "indirect performance effects" in text, text
        assert ("The browser terminal functional smoke runs locally." in text) == ("browser" in expected), text
        assert ("native-workspace" in names) == native, text
        if resize is not None:
            assert ("native-resize" in names) == resize, text
        if guard is not None:
            assert ("guard-contract" in names) == guard, text
        if release is not None:
            assert ("native-release" in names) == release, text
            assert "RELEASE_ACCEPTANCE_SCOPE=" in text, text
        assert {job["os"] for job in plan["ci"]} == ({"ubuntu-latest", "macos-latest"} if native else set()), text
        for job in plan["ci"]:
            assert set(job["checks"]) == {name for name in names if name.startswith("native-")}, text

    both = ["rendering", "browser"]
    measurement_inputs = ["e2e/terminal-resize/benchmark.py", "e2e/terminal-resize/performance-policy.json",
                          "e2e/terminal-resize/gui-benchmark.py", "e2e/terminal-resize/gui-smoke.py", "e2e/terminal-resize/gui-policy.json"]
    cases = [(f"rust/crates/{crate}/src/fixture.rs", [])
             for crate in ["api", "runtime", "tools", "engine-acp", "engine-host", "service-host"]]
    cases += [(path, both) for path in ["rust/crates/rusty-sudocode-cli/src/render.rs",
        "rust/crates/rusty-sudocode-cli/src/repl_ui/layout.rs",
        "rust/crates/pty-harness/src/lib.rs", "rust/crates/rusty-sudocode-cli/tests/common/mod.rs",
        "e2e/terminal-resize/host.cjs"]]
    cases += [("rust/crates/engine-core/src/lib.rs", ["rendering"])]
    cases += [(path, []) for path in ["rust/Cargo.lock", "rust/.cargo/config.toml", "rust/crates/api/Cargo.toml",
        "rust/crates/api/build.rs", "rust/vendor/fixture/src/lib.rs"]]
    cases += [("rust/crates/rusty-sudocode-cli/tests/pty_render_performance.rs", ["rendering"]),
              ("rust/crates/rusty-sudocode-cli/tests/pty_hyperlinks.rs", ["browser"]),
              ("rust/crates/rusty-sudocode-cli/tests/pty_prompt_viewport.rs", ["browser"]),
              ("rust/crates/scode-benchmark-drivers/Cargo.toml", both),
              ("rust/crates/scode-benchmark-drivers/future.rs", both)]
    cases += [("e2e/terminal-resize/benchmark.py", both),
              ("e2e/terminal-resize/performance-policy.json", ["rendering"]),
              ("e2e/terminal-resize/gui-benchmark.py", []),
              ("e2e/terminal-resize/gui-smoke.py", ["browser"]),
              ("e2e/terminal-resize/gui-policy.json", ["browser"])]
    release_inputs = {"rust/Cargo.lock", "rust/.cargo/config.toml", "rust/crates/api/Cargo.toml",
        "rust/crates/api/build.rs", "rust/vendor/fixture/src/lib.rs", "rust/crates/scode-benchmark-drivers/Cargo.toml",
        "rust/crates/rusty-sudocode-cli/src/main.rs", "rust/crates/rusty-sudocode-cli/src/startup.rs",
        "rust/crates/rusty-sudocode-cli/src/cli/config_ui.rs", "rust/crates/runtime/src/config.rs",
        "rust/crates/runtime/src/config/fixture.rs", "rust/crates/rusty-sudocode-cli/tests/pty_model_stream_close.rs",
        "e2e/startup/run.py"}
    cases += [(path, both) for path in ["rust/crates/rusty-sudocode-cli/src/main.rs",
        "rust/crates/rusty-sudocode-cli/src/startup.rs", "rust/crates/rusty-sudocode-cli/src/cli/config_ui.rs"]]
    cases += [(path, []) for path in ["rust/crates/runtime/src/config.rs",
        "rust/crates/runtime/src/config/fixture.rs", "rust/crates/rusty-sudocode-cli/tests/pty_model_stream_close.rs",
        "e2e/startup/run.py"]]
    for index, (path, expected) in enumerate(cases):
        base = command("git", "rev-parse", "HEAD", cwd=repository)
        file = repository / path
        file.parent.mkdir(parents=True, exist_ok=True)
        file.write_text("actual changed input\n")
        command("git", "add", ".", cwd=repository)
        command("git", "commit", "-m", f"Performance input {index}", cwd=repository)
        measured_only = path in measurement_inputs
        expect(base, expected, native=not measured_only, resize=False if measured_only else None,
               release=path in release_inputs)
    # A known measurement input cannot hide a changed SUT, shared terminal host,
    # or an unknown external input. The actual committed diff owns the scope.
    for companion in ["rust/crates/rusty-sudocode-cli/src/render.rs",
                      "e2e/terminal-resize/host.cjs", "e2e/terminal-resize/future-observer.cjs"]:
        base = command("git", "rev-parse", "HEAD", cwd=repository)
        for path in [measurement_inputs[0], companion]:
            file = repository / path
            file.write_text(file.read_text() + "mixed changed input\n" if file.exists() else "unknown input\n")
        command("git", "add", ".", cwd=repository)
        command("git", "commit", "-m", "Mixed measurement and functional inputs", cwd=repository)
        expect(base, both, resize=True)
    base = command("git", "rev-parse", "HEAD", cwd=repository)
    command("git", "mv", "rust/crates/rusty-sudocode-cli/src/render.rs",
            "rust/crates/runtime/src/moved.rs", cwd=repository)
    command("git", "commit", "-m", "Move UI source into another crate", cwd=repository)
    expect(base, both)
    base = command("git", "rev-parse", "HEAD", cwd=repository)
    command("git", "rm", "e2e/terminal-resize/host.cjs", cwd=repository)
    command("git", "commit", "-m", "Delete measurement input", cwd=repository)
    expect(base, both)
    # A rename into the known set still includes its original shared input.
    host = repository / "e2e/terminal-resize/host.cjs"
    host.write_text("shared terminal host\n")
    command("git", "add", ".", cwd=repository)
    command("git", "commit", "-m", "Restore shared terminal input", cwd=repository)
    base = command("git", "rev-parse", "HEAD", cwd=repository)
    command("git", "mv", "-f", "e2e/terminal-resize/host.cjs", measurement_inputs[0], cwd=repository)
    command("git", "commit", "-m", "Rename shared host into measurement-only path", cwd=repository)
    expect(base, both, resize=True)
    base = command("git", "rev-parse", "HEAD", cwd=repository)
    command("git", "rm", measurement_inputs[-1], cwd=repository)
    command("git", "commit", "-m", "Delete a statistical policy", cwd=repository)
    expect(base, ["browser"], native=False, resize=False)
    # Both sides of a rename and mixed input sets must retain release scope.
    for left, right in [("rust/crates/rusty-sudocode-cli/src/startup.rs",
                         "rust/crates/runtime/src/moved_startup.rs"),
                        ("rust/crates/runtime/src/moved_startup.rs",
                         "rust/crates/rusty-sudocode-cli/src/startup.rs")]:
        base = command("git", "rev-parse", "HEAD", cwd=repository)
        command("git", "mv", left, right, cwd=repository)
        command("git", "commit", "-m", "Move startup implementation across release scope", cwd=repository)
        expect(base, both, release=True)
    base = command("git", "rev-parse", "HEAD", cwd=repository)
    for path in ["rust/crates/rusty-sudocode-cli/src/render.rs", "rust/crates/runtime/src/config.rs"]:
        destination = repository / path
        destination.write_text((destination.read_text() if destination.exists() else "") + "mixed startup change\n")
    command("git", "add", ".", cwd=repository)
    command("git", "commit", "-m", "Mixed renderer and startup inputs", cwd=repository)
    expect(base, both, release=True)
    base = command("git", "rev-parse", "HEAD", cwd=repository)
    command("git", "rm", "rust/crates/rusty-sudocode-cli/src/startup.rs", cwd=repository)
    command("git", "commit", "-m", "Delete startup implementation", cwd=repository)
    expect(base, both, release=True)
    print("PASS: public Git planner scopes heavy performance, retains native hosts and sees renames/deletions")
    # This actual-Git fixture is required by CI preflight. Editing it alone
    # must not select product Cargo/PTY builds on all three native hosts.
    history = repository / "e2e/pr-history/run.py"
    history.parent.mkdir(parents=True, exist_ok=True)
    base = command("git", "rev-parse", "HEAD", cwd=repository)
    history.write_text("changed Git history acceptance fixture\n")
    command("git", "add", ".", cwd=repository)
    command("git", "commit", "-m", "Git-only history fixture", cwd=repository)
    expect(base, [], native=False, guard=False)
    for path in ["e2e/pr-history/future.py", "rust/crates/runtime/src/history_fixture.rs"]:
        base = command("git", "rev-parse", "HEAD", cwd=repository)
        destination = repository / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text("actual mixed product or unknown input\n")
        history.write_text(history.read_text() + "mixed fixture edit\n")
        command("git", "add", ".", cwd=repository)
        command("git", "commit", "-m", "History fixture with conservative native input", cwd=repository)
        expect(base, [], native=True, guard=False)
    for left, right in [("e2e/pr-history/run.py", "rust/crates/runtime/src/moved_history.rs"),
                        ("rust/crates/runtime/src/moved_history.rs", "e2e/pr-history/run.py")]:
        base = command("git", "rev-parse", "HEAD", cwd=repository)
        command("git", "mv", left, right, cwd=repository)
        command("git", "commit", "-m", "Move across Git-only fixture boundary", cwd=repository)
        expect(base, [], native=True, guard=False)
    print("PASS: public planner keeps Git-only history acceptance in CI preflight; mixed, unknown and renamed native inputs retain product validation")
    # These three independent executor fixtures already run in scoped PR CI.
    # They do not change enforcement or its pinned native helpers. Unknown
    # future fixtures and every shared contract input remain conservative.
    independent = ["benchmark_targets_acceptance.py", "container_inputs_acceptance.py", "clippy_fail_fast.py"]
    paths = [("e2e/local-acceptance/" + name, False) for name in independent]
    paths += [(name, True) for name in ["e2e/local-acceptance/run.py", "e2e/local-acceptance/future_guard.py",
        ".github/local-acceptance/checks.py", ".github/scripts/local_acceptance.py",
        ".github/scripts/check_pr_history.py",
        "spec/local_acceptance.json", ".github/workflows/local-acceptance.yml"]]
    for path, guard in paths:
        base = command("git", "rev-parse", "HEAD", cwd=repository)
        file = repository / path
        file.parent.mkdir(parents=True, exist_ok=True)
        if file == helper:
            file.write_text(file.read_text() + "\n# Changed shared contract input\n")
        else:
            file.write_text("changed enforcement-scope input\n")
        command("git", "add", ".", cwd=repository)
        command("git", "commit", "-m", f"Enforcement input {path}", cwd=repository)
        expect(base, [], native=False, guard=guard)
    for shared in [False, True]:
        base = command("git", "rev-parse", "HEAD", cwd=repository)
        for name in independent + (["run.py"] if shared else []):
            file = repository / "e2e/local-acceptance" / name
            file.write_text(file.read_text() + "mixed change\n")
        command("git", "add", ".", cwd=repository)
        command("git", "commit", "-m", f"Independent fixtures with shared={shared}", cwd=repository)
        expect(base, [], native=False, guard=shared)
    for source, target in [("benchmark_targets_acceptance.py", "moved_guard.py"),
                           ("moved_guard.py", "benchmark_targets_acceptance.py")]:
        base = command("git", "rev-parse", "HEAD", cwd=repository)
        command("git", "mv", "e2e/local-acceptance/" + source, "e2e/local-acceptance/" + target, cwd=repository)
        command("git", "commit", "-m", "Move across enforcement boundary", cwd=repository)
        expect(base, [], native=False, guard=True)
    base = command("git", "rev-parse", "HEAD", cwd=repository)
    command("git", "rm", "e2e/local-acceptance/clippy_fail_fast.py", cwd=repository)
    command("git", "commit", "-m", "Delete independent fixture input", cwd=repository)
    expect(base, [], native=False, guard=False)
    print("PASS: public Git planner skips unchanged enforcement for three independently gated fixtures; shared/mixed/unknown inputs and both rename directions retain it")


def plan_acceptance(directory):
    """Preview actual committed inputs through the public CLI; never attest a fake host."""
    repository = directory / "plans"
    command("git", "init", "--initial-branch=main", repository, cwd=directory)
    command("git", "config", "user.name", "Plan E2E", cwd=repository)
    command("git", "config", "user.email", "plan@example.invalid", cwd=repository)
    helper = repository / ".github/scripts/local_acceptance.py"
    helper.parent.mkdir(parents=True)
    shutil.copyfile(SOURCE / ".github/scripts/local_acceptance.py", helper)
    policy = repository / "spec/local_acceptance.json"
    policy.parent.mkdir()
    rules = json.loads((SOURCE / "spec/local_acceptance.json").read_text())
    # The planner must understand a future architecture-specific profile before
    # the protected workflow/publisher can safely switch to that new matrix.
    rules["checks"]["native-sdk"] = {"include": ["sdk/typescript/**"],
        "targets": ["linux-x64", "linux-arm64", "macos", "windows"]}
    policy.write_text(json.dumps(rules))
    command("git", "add", ".", cwd=repository)
    command("git", "commit", "-m", "Protected fixture policy", cwd=repository)
    command("git", "checkout", "-b", "sdk", cwd=repository)
    source = repository / "sdk/typescript/src/example.ts"
    source.parent.mkdir(parents=True)
    source.write_text("export const fixture = true;\n")
    command("git", "add", ".", cwd=repository)
    command("git", "commit", "-m", "SDK inputs", cwd=repository)

    def preview(system, architecture, success=True, error=None):
        output = command(sys.executable, helper, "plan", "--base", "main", "--policy-ref", "main",
                         "--host", system, "--architecture", architecture, cwd=repository,
                         success=success, error=error)
        if success:
            assert "Preview only" in output
            return json.loads(next(line.split("=", 1)[1] for line in output.splitlines()
                                   if line.startswith("ACCEPTANCE_PLAN=")))

    for system, architecture, local in [("macos", "x86_64", "macos"), ("windows", "AMD64", "windows"),
                                        ("linux", "x86_64", "linux-x64"), ("linux", "aarch64", "linux-arm64")]:
        plan = preview(system, architecture)
        assert plan["local_checks"] == ["preflight", "native-sdk"], plan
        assert plan["local_targets"] == [{"check": "native-sdk", "targets": [local]}], plan
        assert len(plan["ci"]) == 3, plan
        assert {target for job in plan["ci"] for target in job["targets"]} == {
            "macos", "windows", "linux-x64", "linux-arm64"} - {local}, plan
        assert all(job["family"] == "sdk" and job["checks"] == ["native-sdk"] for job in plan["ci"])
        assert plan["deployment_verified"] is False
    preview("linux", "riscv64", success=False, error="Author host does not cover a required target")
    # Mixed SDK/Rust inputs keep ARM64 SDK coverage without inventing an ARM64
    # Rust workspace job, and retain every selected native check on the other OS.
    source = repository / "rust/crates/runtime/src/example.rs"
    source.parent.mkdir(parents=True)
    source.write_text("pub const FIXTURE: bool = true;\n")
    command("git", "add", ".", cwd=repository)
    command("git", "commit", "-m", "Mixed SDK and native inputs", cwd=repository)
    plan = preview("macos", "x86_64")
    assert len(plan["ci"]) == 5, plan
    native = [job for job in plan["ci"] if job["family"] == "native"]
    assert {job["os"] for job in native} == {"ubuntu-latest", "windows-latest"}
    assert all(job["name"] == f"Complement local host ({job['os']})" for job in native)
    expected = {name for name in plan["local_checks"] if name.startswith("native-")} - {"native-sdk"}
    assert all(set(job["checks"]) == expected for job in native)
    print("PASS: CLI plans expose four author-host splits, Linux architectures and mixed SDK/native coverage")


def enforcement_acceptance(*, profile_compat_only=False):
    with tempfile.TemporaryDirectory(prefix="scode-local-guard-") as temporary:
        directory = Path(temporary)
        if not profile_compat_only:
            scope_acceptance(directory)
            performance_scope_acceptance(directory)
            plan_acceptance(directory)
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
            'from contextlib import contextmanager\n'
            'def nested_python():\n'
            '    return sys.executable\n'
            'def shared_python():\n'
            '    return nested_python()\n'
            'def browser_only_helper():\n'
            '    return "unused browser helper v1"\n'
            'def run(name, root, output, base, execute):\n'
            '    policy_ref, candidate_ref = run.execution_refs\n'
            '    assert policy_ref and len(policy_ref) == 40\n'
            '    assert candidate_ref and len(candidate_ref) == 40\n'
            '    if name == "native-sdk":\n'
            '        return [execute([shared_python(), "-c", '
            '"from pathlib import Path; text=Path(\'sdk/typescript/input.txt\').read_text(); print(text); raise SystemExit(text != \'pass\')"], '
            'root, output / "sdk-process.log")]\n'
            '    if name in {"rendering", "browser"}:\n'
            '        raise RuntimeError(browser_only_helper())\n'
            '    return [execute([shared_python(), "-c", '
            '"from pathlib import Path; text=Path(\'input.txt\').read_text(); print(text); raise SystemExit(text != \'pass\')"], '
            'root, output / "actual-process.log")]\n'
        )
        (work / "spec").mkdir()
        (work / "spec/local_acceptance.json").write_text(json.dumps({
            "version": 1, "repository": "acceptance/fixture", "checks": {
                "real-process": {"include": ["**"]}, "native-sdk": {"include": ["sdk/typescript/**"],
                "targets": ["linux-x64", "linux-arm64", "macos", "windows"]}}
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

        from profile_compat_acceptance import verify_profile_compatibility
        verify_profile_compatibility(directory, work, command)
        if profile_compat_only:
            print("TARGETED_PROFILE_COMPATIBILITY_ONLY: actual Git/SSH/hooks checked; "
                  "remaining enforcement and Cargo scopes were not run. This is not full acceptance.")
            return
        from resume_acceptance import verify_resume
        verify_resume(directory, SOURCE, command)
        from host_lease_acceptance import verify_host_lease
        verify_host_lease(directory, SOURCE, command, FIXTURE_ENV)

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

        # Both stronger and weaker candidate policies must fail the actual
        # push with actionable diagnostics, without publishing a branch/note.
        command("git", "checkout", "-b", "contract-change", cwd=work)
        policy_file = work / "spec/local_acceptance.json"
        original_policy = json.loads(policy_file.read_text())
        before_notes = command("git", "ls-remote", "origin", "refs/notes/local-acceptance", cwd=work)
        for label, checks in [
            ("add-check", {**original_policy["checks"], "additional-check": {"include": ["**"]}}),
            ("remove-checks", {}),
        ]:
            policy_file.write_text(json.dumps({**original_policy, "checks": checks}))
            command("git", "add", ".", cwd=work)
            command("git", "commit", "-m", label, cwd=work)
            rejected = subprocess.run(["git", "push", "origin", "contract-change"], cwd=work,
                                      capture_output=True, text=True, encoding="utf-8", errors="replace", env=FIXTURE_ENV)
            assert rejected.returncode != 0, rejected.stdout + rejected.stderr
            assert 'Changed protected files: ["spec/local_acceptance.json"]' in rejected.stderr, rejected.stderr
            assert "Trusted policy: origin/main (" in rejected.stderr, rejected.stderr
            assert "including added checks" in rejected.stderr, rejected.stderr
            assert 'local-acceptance-hooks/local_acceptance.py" contract-update --remote origin' in rejected.stderr, rejected.stderr
            assert "maintain/admin actor" in rejected.stderr, rejected.stderr
            assert "not a request for the end user to review code" in rejected.stderr, rejected.stderr
            assert "CONTRIBUTING.md#acceptance-contract-changes" in rejected.stderr, rejected.stderr
            assert not command("git", "ls-remote", "origin", "refs/heads/contract-change", cwd=work)
            assert command("git", "ls-remote", "origin", "refs/notes/local-acceptance", cwd=work) == before_notes
        command("git", "checkout", "feature", cwd=work)
        from contract_update_acceptance import verify_contract_update
        verify_contract_update(directory, work, FIXTURE_ENV)
        from frozen_refs_acceptance import verify_frozen_refs
        verify_frozen_refs(directory, work, FIXTURE_ENV)
        print("PASS: added/removed checks both block real pushes with diagnostics and unchanged remote refs")

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

        # Adding an SDK input must execute its selected command before the
        # actual push can succeed. These are process fixtures, not product SDK
        # acceptance; the production profile runs the real npm suite separately.
        sdk = work / "sdk/typescript/input.txt"
        sdk.parent.mkdir(parents=True)
        sdk.write_text("fail")
        command("git", "add", ".", cwd=work)
        command("git", "commit", "-m", "SDK fixture must pass its own process", cwd=work)
        command("git", "push", "origin", "feature", cwd=work, success=False)
        assert command("git", "ls-remote", "origin", "refs/heads/feature", cwd=work).startswith(new_head)
        sdk.write_text("pass")
        command("git", "add", ".", cwd=work)
        command("git", "commit", "-m", "SDK fixture process passes", cwd=work)
        pushed = command("git", "push", "origin", "feature", cwd=work)
        assert "ACCEPTANCE_PLAN=" in pushed and "native-sdk" in pushed, pushed
        sdk_head = command("git", "rev-parse", "HEAD", cwd=work)
        command("git", "fetch", "origin", "+refs/notes/local-acceptance:refs/notes/local-acceptance", cwd=work)
        sdk_note = json.loads(command("git", "notes", "--ref=local-acceptance", "show", sdk_head, cwd=work))
        original = sdk_note["attestations"]["fixture"]
        for omit_sdk in [True, False]:
            payload = json.loads(base64.b64decode(original["payload"]))
            if omit_sdk:
                del payload["checks"]["native-sdk"]
            else:
                payload["plan"]["ci"] = []
            data.write_text(json.dumps(payload))
            signature = data.with_suffix(".json.sig")
            if signature.exists():
                signature.unlink()
            command("ssh-keygen", "-Y", "sign", "-f", key, "-n", "sudocode-local-acceptance", data, cwd=work)
            sdk_note["attestations"]["fixture"] = {"signer": "fixture",
                "payload": base64.b64encode(data.read_bytes()).decode(),
                "signature": base64.b64encode(signature.read_bytes()).decode()}
            data.write_text(json.dumps(sdk_note))
            command("git", "notes", "--ref=local-acceptance", "add", "-f", "-F", data, sdk_head, cwd=work)
            command(sys.executable, helper, "verify", cwd=work, success=False,
                    error="Acceptance check set differs" if omit_sdk else "Signed acceptance plan does not match")
        print("PASS: actual SDK process blocks push; valid signatures cannot omit SDK coverage or alter the plan")

        # A dirty source file cannot be silently included in tested inputs.
        (work / "input.txt").write_text("uncommitted")
        command(sys.executable, helper, "check", cwd=work, success=False)
        print("PASS: actual hook/push, failed process, signed notes, fresh head, tamper, preserved notes and dirty inputs")
        # Reject Git/signature/authorization errors before any Cargo fixture.
        # Passing runs retain every native build, cache and policy guard below.
        print(command(sys.executable, str(Path(__file__).with_name("dependency_scope_acceptance.py")),
                      "--output", directory / "dependency-scope", cwd=SOURCE))
        print(command(sys.executable, str(Path(__file__).with_name("vfs_search_cache_acceptance.py")),
                      "--output", directory / "vfs-search-cache", cwd=SOURCE))
        print(command(sys.executable, str(Path(__file__).with_name("cache_acceptance.py")), cwd=SOURCE))
        print(command(sys.executable, str(Path(__file__).with_name("policy_guard_acceptance.py")), cwd=SOURCE))


def main():
    sys.stdout.reconfigure(encoding="utf-8", line_buffering=True)
    sys.stderr.reconfigure(encoding="utf-8", line_buffering=True)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture-child", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--profile-compat-only", action="store_true",
                        help="Debug real profile Git/SSH compatibility only; not full acceptance")
    args = parser.parse_args()
    if args.fixture_child:
        enforcement_acceptance(profile_compat_only=args.profile_compat_only)
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
        child = [sys.executable, str(Path(__file__).resolve()), "--fixture-child"]
        if args.profile_compat_only:
            child.append("--profile-compat-only")
        result = subprocess.run(child,
                                cwd=parent, env=environment)
        assert result.returncode == 0, "Acceptance failed with a real inherited hook environment"
        assert all((parent / ".git" / name).read_bytes() == data for name, data in before.items())
        assert command("git", "rev-parse", "HEAD", cwd=parent) == head
        assert not command("git", "status", "--porcelain", cwd=parent)
        print("PASS: inherited hook environment leaves caller HEAD, index, config and source unchanged")


if __name__ == "__main__":
    main()
