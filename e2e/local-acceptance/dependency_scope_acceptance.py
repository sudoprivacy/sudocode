"""Exercise development-only build selection through real Git, Cargo and CLI."""

import argparse
import ast
from contextlib import redirect_stdout
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys


SOURCE = Path(__file__).resolve().parents[2]
PRODUCT = {"native-context", "native-services", "native-cohost"}


def git_for_windows_bash(path=None):
    """Git for Windows' own Bash, located from the git on PATH and never WSL's.

    A git hook runs with Git's mingw64/libexec/git-core first on PATH, three
    levels below the bin/bash.exe of the same installation, so look in every
    ancestor of the resolved git rather than its parent and grandparent only.
    """
    git = shutil.which("git", path=path)
    if not git:
        return None
    for directory in Path(git).resolve().parents:
        for candidate in (directory / "bash.exe", directory / "bin" / "bash.exe"):
            if candidate.is_file():
                return str(candidate)
    return None


def check_compiler_priority(executor, rustc, output, environment, expected):
    """Hold a real metadata compile on its source pipe until it is observed."""
    driver = output / "compiler-priority-probe.py"
    driver.write_text('''import subprocess
import sys
import time

import psutil

rustc, artifact, expected = sys.argv[1:]
with subprocess.Popen([rustc, "--crate-name", "scope_priority_probe", "--crate-type", "lib",
                       "--emit", "metadata", "-o", artifact, "-"], stdin=subprocess.PIPE) as compiler:
    process = psutil.Process(compiler.pid)
    started = time.monotonic()
    deadline = started + 10
    try:
        # A surrounding executor can lower the same child first. Leave time
        # for this executor's own 150 ms observer to record it as well.
        while time.monotonic() - started < 0.5 or process.nice() != int(expected):
            if time.monotonic() >= deadline:
                raise RuntimeError("The active compiler did not reach the expected priority")
            time.sleep(0.025)
        compiler.communicate(b"pub fn value() -> u8 { 42 }\\n", timeout=10)
        if compiler.returncode != 0:
            raise RuntimeError(f"The observed metadata compile exited {compiler.returncode}")
    finally:
        if compiler.poll() is None:
            compiler.kill()
            compiler.wait()
''', encoding="utf-8")
    artifact = output / "compiler-priority-probe.rmeta"
    result = executor.run_command([sys.executable, driver, rustc, artifact, expected],
                                  executor.ROOT, output / "compiler-priority-probe.log", environment)
    assert artifact.stat().st_size > 0
    assert result["compiler_priorities"].get(rustc.name, {}).get(str(expected), 0) > 0, result
    return result


def pinned_cache_command(output, environment):
    """Run the Linux cache command with and without an unused installed Rust."""
    tools = output / "cache-tools"
    tools.mkdir()
    rustup = tools / "rustup"
    rustup.write_text('''#!/usr/bin/env bash
case "$*" in
  "show active-toolchain") printf '%s (directory override)\\n' "$FIXTURE_ACTIVE" ;;
  "toolchain list --quiet") printf '%s\\n' "$FIXTURE_INVENTORY" ;;
  *) printf '%s\\n' "$@"; exit 17 ;;
esac
''', encoding="utf-8")
    rustup.chmod(0o755)
    script = SOURCE / ".github/actions/rust-toolchain/cache-command.mjs"
    pin = "1.100.0"
    active = pin + "-x86_64-unknown-linux-gnu"
    extra = "1.99.1-x86_64-unknown-linux-gnu"
    inventory = active + "\n" + extra
    fixture = {**environment, "PATH": str(tools) + os.pathsep + environment["PATH"],
               "FIXTURE_ACTIVE": active, "FIXTURE_INVENTORY": inventory}
    node = shutil.which("node", path=fixture["PATH"])
    assert node, "Linux cache command acceptance requires Node"

    def versions(current):
        return subprocess.check_output([node, script, pin, "rustup", "toolchain", "list", "--quiet"],
                                       env=current).decode().splitlines()

    assert versions(fixture) == versions({**fixture, "FIXTURE_INVENTORY": active}) == [active]
    assert versions({**fixture, "FIXTURE_ACTIVE": extra}) == [active, extra]
    result = subprocess.run([node, script, pin, "rustup", "run", active, "rustc", "-vV"],
                            env=fixture, capture_output=True)
    assert result.returncode == 17 and result.stdout.decode().splitlines() == ["run", active, "rustc", "-vV"]
    preinstalled = SOURCE / ".github/actions/rust-toolchain/preinstalled-cache-command.mjs"
    host = "x86_64-unknown-linux-gnu"
    assert subprocess.check_output([node, preinstalled, host, "rustup", "toolchain", "list", "--quiet"],
                                   env=fixture, timeout=10).decode().strip() == f"stable-{host}"
    call = [node, preinstalled, host, node, "-e", "process.stdout.write(process.env.RUSTUP_TOOLCHAIN);process.exitCode=17"]
    result = subprocess.run(call, capture_output=True, timeout=10)
    assert result.returncode == 17 and result.stdout.decode() == f"stable-{host}"
    print("PASS: the cache command binds the active pin, preserves conservative inventory, and forwards command results",
          flush=True)


def preinstalled_cache_probe(output):
    from unittest.mock import patch

    output.mkdir()
    probe = SOURCE / ".github/actions/rust-toolchain/preinstalled-cache.py"
    spec = importlib.util.spec_from_file_location("preinstalled_cache", probe)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    host = "x86_64-pc-windows-msvc"
    commit = "a" * 40
    compiler = f"release: 1.100.0\nhost: {host}\ncommit-hash: {commit}\n"
    installed = "\n".join(f"{name}-{host}" for name in ("rustc", "cargo", "rust-std", "clippy", "rustfmt"))
    (output / "rust-toolchain.toml").write_text(
        '[toolchain]\nchannel = "1.100.0"\nprofile = "minimal"\ncomponents = ["clippy", "rustfmt"]\n', encoding="utf-8")
    environment = {"GITHUB_WORKSPACE": str(output), "RUNNER_OS": "Windows", "RUNNER_ARCH": "X64",
                   "USERPROFILE": r"C:\Users\runneradmin"}
    with patch.object(module.subprocess, "check_output", side_effect=[compiler, installed]) as metadata, \
            patch.object(module, "verified_release", return_value={"git_commit_hash": commit}):
        values = module.lookup_inputs(environment)
        assert values["cargo-home"] == r"C:\Users\runneradmin\.cargo"
        assert values["dependency-cache-command"].endswith(f'preinstalled-cache-command.mjs" {host} {{0}}')
        assert all(call.kwargs["env"]["RUSTUP_AUTO_INSTALL"] == "0" for call in metadata.call_args_list)
    for observed, components, expected in (
        (compiler.replace("1.100.0", "1.99.0"), installed, commit),
        (compiler.replace(host, "aarch64-pc-windows-msvc"), installed, commit),
        (compiler, installed.replace(f"clippy-{host}", ""), commit),
        (compiler, installed, "b" * 40),
    ):
        with patch.object(module.subprocess, "check_output", side_effect=[observed, components]), \
                patch.object(module, "verified_release", return_value={"git_commit_hash": expected}):
            try:
                module.lookup_inputs(environment)
            except ValueError:
                continue
            raise AssertionError("An inexact preinstalled Rust qualified for a cache lookup")
    with patch.dict(os.environ, {**environment, "RUSTUP_TOOLCHAIN": "stable", "GITHUB_OUTPUT": str(output / "outputs")}):
        module.main()
    assert not (output / "outputs").exists()


def workflow_scope(output, environment):
    """Execute the actual workflow guards and bind their conditional steps."""
    output.mkdir()
    if os.name == "nt":
        # CreateProcess can resolve a bare bash against the parent's PATH,
        # selecting WSL even when the explicit child environment lists Git first.
        bash = git_for_windows_bash(environment.get("PATH"))
    else:
        bash = shutil.which("bash", path=environment.get("PATH"))
    if not bash:
        raise RuntimeError("Workflow guard acceptance requires Bash; on Windows install Git for Windows with Bash")
    workflow = (SOURCE / ".github/workflows/rust-ci.yml").read_text(encoding="utf-8")
    lines = workflow.splitlines()
    cache_command = "          cmd-format: ${{ steps.rust_toolchain.outputs.dependency-cache-command }}"
    toolchain = (SOURCE / ".github/actions/rust-toolchain/action.yml").read_text(encoding="utf-8")
    assert "value: ${{ steps.install.outputs.dependency-cache-command }}" in toolchain
    assert "echo 'dependency-cache-command={0}'" in toolchain
    publisher = (SOURCE / ".github/workflows/ci-dependency-cache.yml").read_text(encoding="utf-8")
    assert publisher.count(cache_command) == 2
    early = publisher.split("        id: preinstalled_lookup\n", 1)[1].split("      - uses: ./.github/actions/rust-toolchain", 1)[0]
    assert "          lookup-only: true\n          save-if: false\n" in early
    assert "          cmd-format: ${{ steps.preinstalled.outputs.dependency-cache-command }}" in early
    assert "        if: steps.preinstalled_lookup.outcome != 'success' || steps.preinstalled_lookup.outputs.cache-hit != 'true'" in publisher
    assert "        if: steps.rust_toolchain.outcome == 'success'\n        id: lookup" in publisher
    missing_key = "steps.lookup.outcome == 'success' && steps.lookup.outputs.cache-hit != 'true'"
    assert publisher.count(missing_key) == 3
    preinstalled_cache_probe(output / "preinstalled-cache-probe")
    cache_test = lines.index("      - name: Exercise the pinned dependency cache command")
    assert lines[cache_test + 1] == "        if: github.event_name == 'pull_request' && steps.scope.outputs.policy_fixtures_required == 'true'"
    assert "--pinned-cache-command-only" in lines[cache_test + 2]
    # Bind the real native guard cases below to the production queue wiring.
    # The PR must not allocate a duplicate lint matrix, while the final gate
    # must continue to wait for all three native platforms.
    lint = re.search(r"^  clippy-workspace:\n.*?(?=^  [a-z]|\Z)", workflow, re.M | re.S).group()
    predicate = lint.split("    if: >-\n", 1)[1].split("    needs:", 1)[0]
    assert " ".join(predicate.split()) == (
        "!inputs.live_only && !inputs.cache_only && "
        "needs.one-nexus-vfs-pin.outputs.clippy_required == 'true' && "
        "github.event_name == 'merge_group'"), predicate
    assert "        os: [ubuntu-latest, windows-latest, macos-latest]" in lint
    assert "      fail-fast: true" in lint
    assert lint.count(cache_command) == 1
    gate = re.search(r"^  integrated-acceptance:\n.*?(?=^  [a-z]|\Z)", workflow, re.M | re.S).group()
    assert "    if: always() && github.event_name == 'merge_group'" in gate
    assert "    needs: [one-nexus-vfs-pin, a2a-live, cohost-live, clippy-workspace]" in gate
    for job, flag in [("a2a-live", "a2a"), ("cohost-live", "cohost")]:
        block = re.search(rf"^  {job}:\n.*?(?=^  [a-z]|\Z)", workflow, re.M | re.S).group()
        assert "    needs: one-nexus-vfs-pin\n" in block
        assert f"    if: github.event_name == 'merge_group' && needs.one-nexus-vfs-pin.outputs.{flag}_required == 'true'" in block
        assert block.count(cache_command) == 1
    assert "          COHOST_RESULT: ${{ needs.cohost-live.result }}" in gate
    branches = workflow.split("  push:\n    branches:\n", 1)[1].split("    paths:", 1)[0]
    assert {line.strip() for line in branches.splitlines() if line.strip().startswith("- ")} == {
        "- 'gaebal/**'", "- 'omx-issue-*'"}, branches
    assert "  pull_request:\n" in workflow and "  merge_group:\n" in workflow
    assert "full_preflight" not in workflow, "Every remaining event must retain full metadata checks"
    for filename in ["ci-dependency-cache.yml", "ci-vfs-search-cache.yml"]:
        publisher = (SOURCE / ".github/workflows" / filename).read_text(encoding="utf-8")
        assert "  push:\n    branches: [main]\n" in publisher, filename

    def step(name):
        matches = [index for index, line in enumerate(lines) if line == "      - name: " + name]
        assert len(matches) == 1, name
        start = matches[0]
        end = start + 1
        while end < len(lines) and (not lines[end].strip() or
                                   (lines[end].startswith("      ") and not lines[end].startswith("      - "))):
            end += 1
        return lines[start:end]

    condition = "        if: needs.one-nexus-vfs-pin.outputs.cohost_required == 'true'"
    for name in ["Co-host duet against a daemon built from this checkout",
                 "Conditional owner content survives a co-host process restart", "Checkout the VFS contract used by the co-host",
                 "Search authorization and restart under the confined co-host owner", "The driver-ai co-host still resolves"]:
        assert [line for line in step(name) if line.startswith("        if:")] == [condition], name
    assert "      cohost_required: ${{ steps.scope.outputs.cohost_required }}" in lines
    assert "      a2a_required: ${{ steps.scope.outputs.a2a_required }}" in lines
    journeys = ["Install native durability fault injector", "A2A round-trip against the pinned daemon",
                "Cross-node A2A against two nodes", "Auth-on A2A — the node decides who a message is from"]
    for name in journeys:
        assert [line for line in step(name) if line.startswith("        if:")] == [
            "        if: needs.one-nexus-vfs-pin.outputs.a2a_required == 'true'"], name
    mailbox_upload = "Retain mailbox fault inventory and exact case results"
    syscall_upload = "Retain native syscall fault evidence"
    order = [lines.index(step(name)[0]) for name in [*journeys, mailbox_upload, syscall_upload]]
    assert order == sorted(order), "A2A checks must run before the evidence uploads"
    assert "        if: always() && needs.one-nexus-vfs-pin.outputs.a2a_required == 'true'" in step(mailbox_upload)
    assert "        if: failure() && needs.one-nexus-vfs-pin.outputs.a2a_required == 'true'" in step(
        syscall_upload)
    assert not any("continue-on-error:" in line for name in (mailbox_upload, syscall_upload) for line in step(name))
    for name in ["Explain selected daemon acceptance", "Require native daemon acceptance and zero workspace warnings"]:
        assert "          A2A_REQUIRED: ${{ needs.one-nexus-vfs-pin.outputs.a2a_required }}" in step(name)
        assert "          COHOST_REQUIRED: ${{ needs.one-nexus-vfs-pin.outputs.cohost_required }}" in step(name)

    def script(name):
        block = step(name)
        start = block.index("        run: |") + 1
        body = []
        for line in block[start:]:
            if not line.strip() or line.lstrip().startswith("#"):
                continue
            assert line.startswith("          "), (name, line)
            body.append(line[10:])
        return "set -eu\n" + "\n".join(body) + "\n"

    scripts = {"explain": script("Explain selected daemon acceptance"),
               "cohost-explain": script("Explain selected co-host acceptance"),
               "required": script("Require native daemon acceptance and zero workspace warnings")}
    for name, source in scripts.items():
        (output / f"{name}.sh").write_text(source, encoding="utf-8")
    cases = [
        ("explain", "true", "true", "true", "success", True),
        ("explain", "false", "true", "true", "success", True),
        ("explain", "true", "false", "true", "success", True),
        ("explain", "", "true", "true", "success", False),
        ("explain", "true", "", "true", "success", False),
        ("required", "true", "true", "true", "success", True),
        ("required", "false", "true", "true", "success", True),
        ("required", "true", "false", "true", "success", True),
        ("required", "false", "false", "false", "skipped", True),
        ("required", "true", "false", "false", "skipped", False),
        ("required", "false", "true", "false", "skipped", False),
        ("required", "false", "false", "true", "success", False),
        ("required", "", "true", "true", "success", False),
        ("required", "true", "", "true", "success", False),
        ("required", "false", "true", "true", "skipped", False),
        ("required", "true", "true", "true", "failure", False),
    ]
    cases = [(name, {"A2A_REQUIRED": a2a, "COHOST_REQUIRED": cohost,
                     "DAEMON_REQUIRED": daemon,
                     "A2A_RESULT": actual if a2a == "true" else "skipped",
                     "COHOST_RESULT": actual if cohost == "true" else "skipped"}, succeeds)
             for name, a2a, cohost, daemon, actual, succeeds in cases]
    cases.extend(("cohost-explain", {"COHOST_REQUIRED": value}, succeeds)
                 for value, succeeds in [("true", True), ("false", False), ("", False)])
    cases.extend(("required", {"A2A_REQUIRED": a2a, "COHOST_REQUIRED": cohost,
                              "DAEMON_REQUIRED": "true", "A2A_RESULT": cli_result,
                              "COHOST_RESULT": cohost_result}, False)
                 for a2a, cohost, cli_result, cohost_result in [
                     ("true", "true", "success", "failure"),
                     ("true", "true", "cancelled", "success"),
                     ("true", "false", "success", "success"),
                     ("false", "true", "success", "success"),
                     ("true", "true", "skipped", "success"),
                     ("true", "true", "success", "cancelled"),
                     ("true", "true", "success", ""),
                     ("true", "true", "", "success"),
                 ])
    cases.extend(("required", {"A2A_REQUIRED": "false", "COHOST_REQUIRED": "false",
                              "DAEMON_REQUIRED": "false", "A2A_RESULT": "skipped",
                              "COHOST_RESULT": "skipped", **values}, succeeds)
                 for values, succeeds in [
                     ({"CLIPPY_REQUIRED": "true", "CLIPPY_RESULT": "success"}, True),
                     ({"CLIPPY_REQUIRED": "true", "CLIPPY_RESULT": "failure"}, False),
                     ({"CLIPPY_REQUIRED": "true", "CLIPPY_RESULT": "skipped"}, False),
                     ({"CLIPPY_REQUIRED": "true", "CLIPPY_RESULT": "cancelled"}, False),
                     ({"CLIPPY_REQUIRED": "false", "CLIPPY_RESULT": "success"}, False),
                     ({"CLIPPY_REQUIRED": "", "CLIPPY_RESULT": "skipped"}, False),
                     ({"CLIPPY_REQUIRED": "true", "CLIPPY_RESULT": "success", "PIN_RESULT": "failure"}, False),
                 ])
    records = []
    for index, (name, overrides, succeeds) in enumerate(cases):
        values = {"PIN_RESULT": "success", "CLIPPY_REQUIRED": "false", "CLIPPY_RESULT": "skipped", **overrides}
        result = subprocess.run([bash, "-c", scripts[name]], env={**environment, **values}, capture_output=True)
        log = output / f"{index:02}.log"
        # Native setup failures can use a different encoding. Retain their raw
        # output so a decoder exception never hides the actual command result.
        log.write_bytes(result.stdout + result.stderr)
        assert (result.returncode == 0) == succeeds, (values, result.stdout, result.stderr)
        records.append({"script": name, "values": values, "exit_code": result.returncode,
                        "log_sha256": hashlib.sha256(log.read_bytes()).hexdigest()})
    managed_name = "Managed scode against the public nexusd-cluster"
    assert [line for line in step(managed_name) if line.startswith("        if:")] == [condition]
    assert "          A2A_REQUIRED: ${{ needs.one-nexus-vfs-pin.outputs.a2a_required }}" in step(managed_name)
    cli_entry = (SOURCE / "e2e/nexus-a2a/run.sh").read_text(encoding="utf-8")
    assert cli_entry.rstrip().endswith('authon_stop\nbash ./run-managed-subprocess.sh\n\necho "E2E OK"')
    managed = script(managed_name)
    (output / "managed.sh").write_text(managed, encoding="utf-8")
    work = output / "managed-workflow"
    entry = work / "e2e/nexus-a2a/run-managed-subprocess.sh"
    entry.parent.mkdir(parents=True)
    entry.write_text('#!/usr/bin/env bash\nset -eu\nprintf "managed\\n" >> "$MANAGED_CALLS"\nexit "$MANAGED_EXIT"\n',
                     encoding="utf-8")
    entry.chmod(0o755)
    managed_records = []
    for index, (selected, exit_code, calls, succeeds) in enumerate([
            ("true", 0, 0, True), ("false", 0, 1, True), ("false", 19, 1, False),
            ("", 0, 0, False), ("unknown", 0, 0, False)]):
        called = output / f"managed-{index}.calls"
        result = subprocess.run([bash, "-c", managed], cwd=work, capture_output=True,
            env={**environment, "A2A_REQUIRED": selected, "MANAGED_EXIT": str(exit_code),
                 "MANAGED_CALLS": called.as_posix()})
        log = output / f"managed-{index}.log"
        log.write_bytes(result.stdout + result.stderr)
        actual = len(called.read_text().splitlines()) if called.exists() else 0
        assert actual == calls and (result.returncode == 0) == succeeds, (selected, actual, result.returncode)
        managed_records.append({"a2a_required": selected, "managed_calls": actual,
                                "exit_code": result.returncode,
                                "log_sha256": hashlib.sha256(log.read_bytes()).hexdigest()})
    (output / "managed-result.json").write_text(json.dumps(managed_records, indent=2) + "\n")
    (output / "result.json").write_text(json.dumps(records, indent=2) + "\n")
    return {"actual_workflow_guard_cases": len(records), "managed_dispatch_cases": len(managed_records),
            "managed_required_once_with_independent_cohost_coverage": True, "cohost_steps_bound_to_selector": True,
            "CLI_and_cohost_steps_bound_to_independent_selectors": True,
            "CLI_and_cohost_jobs_have_no_mutual_dependency": True,
            "three_platform_lint_required_once_on_queue": True,
            "main_preflight_removed_and_independent_cache_publishers_retained": True}


def scheduling_scope(output, environment, *, metadata_only=False):
    """Run the actual selector on committed scheduling edits and conservative controls."""
    output.mkdir()
    repository = output / "repository"
    repository.mkdir()
    records, commands = [], []

    def git(*args, data=None, overrides=None):
        argv = ["git", *map(str, args)]
        result = subprocess.run(argv, cwd=repository, input=data, capture_output=True,
                                env={**environment, **(overrides or {})})
        index = len(commands)
        (output / f"git-{index:03}.stdout").write_bytes(result.stdout)
        (output / f"git-{index:03}.stderr").write_bytes(result.stderr)
        commands.append({"argv": argv, "exit_code": result.returncode})
        assert result.returncode == 0, result.stderr.decode(errors="replace")
        return result.stdout.decode().strip()

    def write(name, contents):
        path = repository / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents, encoding="utf-8", newline="\n")

    runner = ".github/scripts/local_acceptance.py"
    selector = ".github/scripts/ci_input_scope.py"
    profiles = ".github/local-acceptance/checks.py"
    names = [runner, selector, profiles, ".github/local-acceptance/Dockerfile", "spec/local_acceptance.json",
             "e2e/local-acceptance/resource_scheduling_acceptance.py",
             "e2e/local-acceptance/host_lease_acceptance.py"]
    sources = {name: (SOURCE / name).read_text(encoding="utf-8") for name in names}
    git("init", "--initial-branch=main")
    git("config", "user.name", "Scheduling Scope E2E")
    git("config", "user.email", "scheduling-scope@example.invalid")
    configuration = subprocess.run(["git", "config", "--get", "localAcceptance.vsdevcmd"],
        cwd=SOURCE, env=environment, capture_output=True, text=True).stdout.strip()
    if configuration:
        git("config", "localAcceptance.vsdevcmd", configuration)
    for name, contents in sources.items():
        write(name, contents)
    git("add", ".")
    git("commit", "-m", "Original committed scheduling scope inputs")
    base = git("rev-parse", "HEAD")

    def insert_body(source, name, statement="pass"):
        node = next(node for node in ast.parse(source).body
                    if isinstance(node, ast.FunctionDef) and node.name == name)
        lines = source.splitlines(keepends=True)
        lines.insert(node.body[0].lineno - 1, "    " + statement + "\n")
        return "".join(lines)

    def observe(label, first, head, expected, *, rust=False, kind="scheduling", clippy=None, events=None, overrides=None):
        for event_name in events or ["pull_request", "push", "merge_group"]:
            if event_name == "pull_request":
                event = {"pull_request": {"base": {"sha": first}, "head": {"sha": head}}}
            elif event_name == "push":
                event = {"before": first, "after": head}
            else:
                event = {"merge_group": {"base_sha": first, "head_sha": head}}
            prefix = output / f"{label}-{event_name}"
            event_path, github_output = prefix.with_suffix(".event.json"), prefix.with_suffix(".output")
            event_path.write_text(json.dumps(event), encoding="utf-8")
            github_output.touch()
            argv = [sys.executable, str(SOURCE / ".github/scripts/ci_input_scope.py")]
            result = subprocess.run(argv, cwd=repository, env={**environment, **(overrides or {}),
                "GITHUB_EVENT_NAME": event_name, "GITHUB_EVENT_PATH": str(event_path),
                "GITHUB_OUTPUT": str(github_output)}, capture_output=True)
            prefix.with_suffix(".stdout").write_bytes(result.stdout)
            prefix.with_suffix(".stderr").write_bytes(result.stderr)
            assert result.returncode == 0, result.stderr.decode(errors="replace")
            actual = json.loads(result.stdout)
            assert actual["daemon_required"] == str(not expected).lower(), (label, actual)
            assert actual["a2a_required"] == actual["cohost_required"] == actual["daemon_required"], (label, actual)
            if kind == "scheduling":
                assert actual["live_required"] == actual["clippy_required"] == str(rust).lower(), (label, actual)
                if expected:
                    assert actual["acceptance_scheduling_scope"]["decision"] == "unchanged_queue_executors", actual
                    assert actual["acceptance_scheduling_scope"]["contract_guard_required"] is True, actual
            else:
                assert actual["live_required"] == str(rust).lower(), (label, actual)
                assert actual["clippy_required"] == str(rust if clippy is None else clippy).lower(), (label, actual)
                if expected:
                    assert actual["acceptance_metadata_scope"]["decision"] == "unchanged_queue_executors", actual
                    assert actual["acceptance_metadata_scope"]["fingerprint_source"] == "comparison_base", actual
                    assert actual["acceptance_metadata_scope"]["comparison_base"] == first, actual
                    assert actual["acceptance_metadata_scope"]["comparison_base_kind"] == {
                        "merge_group": "protected_merge_queue_base", "pull_request": "target_merge_base",
                        "push": "push_previous_commit"}[event_name], actual
                    assert actual["acceptance_metadata_scope"]["contract_guard_required"] is True, actual
                    assert actual["acceptance_metadata_scope"]["generic_queue_daemon_smoke_omitted"] is True, actual
            assert actual["benchmark_required"] == actual["container_required"] == "true", actual
            records.append({"case": label, "event": event_name, "base": first, "head": head,
                            "expected_scheduling_only": expected, "actual": actual,
                            "stdout_sha256": hashlib.sha256(result.stdout).hexdigest()})

    pure = insert_body(sources[runner], "host_acceptance_lease")
    changed_fixture = sources[names[-1]] + "\n# Private enforcement-fixture input.\n"
    changed_pins = sources[profiles]
    for name, contents in [(runner, pure), (names[-1], changed_fixture)]:
        changed_pins, count = re.subn(r'("' + re.escape(name) + r'": ")[0-9a-f]{64}(")',
            lambda match: match[1] + hashlib.sha256(contents.encode()).hexdigest() + match[2], changed_pins)
        assert count == 1, name
    missing = sources[runner].splitlines(keepends=True)
    declaration = next(node for node in ast.parse(sources[runner]).body
                       if isinstance(node, ast.FunctionDef) and node.name == "host_acceptance_lease")
    first_line = min([declaration.lineno, *[node.lineno for node in declaration.decorator_list]])
    del missing[first_line - 1:declaration.end_lineno]
    cases = [
        ("lease-body", {runner: pure}, True),
        ("check-body", {runner: insert_body(sources[runner], "perform_check")}, True),
        ("reservation-body", {runner: insert_body(sources[runner], "profile_reservations")}, True),
        ("fixture-and-pins", {runner: pure, profiles: changed_pins, names[-1]: changed_fixture}, True),
        ("command-body", {runner: insert_body(sources[runner], "run_command")}, False),
        ("selection-body", {runner: insert_body(sources[runner], "selected")}, False),
        ("module-global", {runner: pure + "\nSCHEDULING_SCOPE_CONTROL = True\n"}, False),
        ("changed-signature", {runner: sources[runner].replace(
            "def host_acceptance_lease(attempt, head):", "def host_acceptance_lease(attempt, head=None):")}, False),
        ("changed-decorator", {runner: sources[runner].replace(
            "@contextlib.contextmanager\ndef host_acceptance_lease", "@((lambda function: function))\ndef host_acceptance_lease")}, False),
        ("reservation-default", {runner: sources[runner].replace(
            "def profile_reservations(ordered, pending):", "def profile_reservations(ordered, pending=None):")}, False),
        ("reservation-annotation", {runner: sources[runner].replace(
            "def profile_reservations(ordered, pending):", "def profile_reservations(ordered: int, pending):")}, False),
        ("missing-scheduling-function", {runner: "".join(missing)}, False),
        ("duplicate-definition", {runner: pure + "\ndef host_acceptance_lease(attempt, head):\n    pass\n"}, False),
        ("unknown-mixed-input", {runner: pure, ".github/unrecognized.py": "# Conservative mixed input\n"}, False),
        ("mixed-product-source", {runner: pure, "rust/crates/runtime/src/scheduling_scope.rs": "// Product input\n"}, False),
        ("changed-profile-execution", {runner: pure, profiles: insert_body(sources[profiles], "run")}, False),
        ("unknown-profile-source", {runner: pure, profiles: "def broken(\n"}, False),
    ]
    if not metadata_only:
        # Derive the required new review with the original verifier, before
        # changing either registry. The selector later sees only Git objects.
        write(runner, pure)
        git("add", ".")
        git("commit", "-m", "Scheduling control before registration")
        unregistered = git("rev-parse", "HEAD")
        spec = importlib.util.spec_from_file_location("original_registration_verifier", SOURCE / runner)
        protected = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(protected)
        protected.ROOT = repository
        diagnostic = io.StringIO()
        with redirect_stdout(diagnostic):
            try:
                protected.resource_executor_identity(unregistered)
            except ValueError as error:
                assert str(error) == "Unreviewed complete executor program; full current checks remain required"
            else:
                raise AssertionError("New scheduling source requires a fresh layout registration")
        prefix = "LOCAL_ACCEPTANCE_RESOURCE_SOURCE_REFUSED="
        line, = diagnostic.getvalue().splitlines()
        assert line.startswith(prefix)
        layout = json.loads(line[len(prefix):])["observed_source_layout"]
        registration = "    reviewed_sources = {\n"
        assert pure.count(registration) == 1
        registered = pure.replace(registration, registration + f'        "{layout}",\n', 1)

        def runner_pin(source):
            blob = git("hash-object", "--stdin", data=source.encode("utf-8"))
            modified, count = re.subn(
                r'("\.github/scripts/local_acceptance\.py":\s*\[\s*"100644",\s*"blob",\s*")[a-f0-9]{40}("\s*\])',
                lambda match: match[1] + blob + match[2], sources[selector])
            assert count == 1
            return modified

        base_selector = runner_pin(sources[runner])
        # Bind the fixture baseline to the actual tracked runner bytes too.
        git("switch", "--detach", base)
        write(selector, base_selector)
        if git("status", "--porcelain"):
            git("add", ".")
            git("commit", "-m", "Register original private runner Git blob")
        original = git("rev-parse", "HEAD")
        wrong_layout = registered.replace(layout, "1" * 64, 1)
        runner_changed = insert_body(registered, "run_command")
        extra_layout = registered.replace(registration, registration + '        "' + "2" * 64 + '",\n', 1)
        for label, source, selector_source, expected in [
            ("registered-scheduling-edit", registered, runner_pin(registered), True),
            ("stale-selector-registration", registered, runner_pin(pure), False),
            ("selector-code-changed", registered,
             insert_body(runner_pin(registered), "main"), False),
            ("missing-resource-registration", pure, runner_pin(pure), False),
            ("wrong-resource-registration", wrong_layout, runner_pin(wrong_layout), False),
            ("runner-code-changed", runner_changed, runner_pin(runner_changed), False),
            ("extra-resource-registration", extra_layout, runner_pin(extra_layout), False),
        ]:
            git("switch", "--detach", original)
            write(runner, source)
            write(selector, selector_source)
            git("add", ".")
            git("commit", "-m", "Registered scheduling scope " + label)
            observe(label, original, git("rev-parse", "HEAD"), expected,
                    events=None if expected else ["merge_group"])

        for label, edits, expected in cases:
            git("switch", "--detach", base)
            for name, contents in edits.items():
                write(name, contents)
            git("add", ".")
            git("commit", "-m", "Scope control " + label)
            observe(label, base, git("rev-parse", "HEAD"), expected, rust=label == "mixed-product-source")
        for label, declaration in [
            ("external-reference", "def external_control():\n    return profile_reservations\n"),
            ("external-namespace", 'def external_control():\n    return globals().get("profile_" + "reservations")\n'),
            ("external-exec", 'def external_control(current_runner, namespace):\n'
                              '    exec(compile(current_runner, namespace["__file__"], "exec"), namespace)\n'),
        ]:
            git("switch", "--detach", base)
            write(runner, sources[runner] + "\n" + declaration)
            git("add", ".")
            git("commit", "-m", "Unchanged external observer " + label)
            before = git("rev-parse", "HEAD")
            write(runner, pure + "\n" + declaration)
            git("add", ".")
            git("commit", "-m", "Scheduling edit with existing external observer " + label)
            observe(label, before, git("rev-parse", "HEAD"), False)
        git("switch", "--detach", base)
        # Commit mode-only trees through private indexes; no OS symlink privilege
        # or mutation of a caller's worktree/index is needed.
        for mode in ["100755", "120000"]:
            index = output / (mode + ".index")
            values = {"GIT_INDEX_FILE": str(index)}
            git("read-tree", base, overrides=values)
            blob = git("rev-parse", base + ":" + runner)
            git("update-index", "--cacheinfo", f"{mode},{blob},{runner}", overrides=values)
            tree = git("write-tree", overrides=values)
            head = git("commit-tree", tree, "-p", base, "-m", "Committed mode boundary " + mode)
            observe("mode-" + mode, base, head, False)
    metadata_functions = ["commands_hash", "profile_commands_hashes", "reachable_profile_commands_hashes",
                          "resource_executor_identity", "resource_equivalent_profiles", "compatible_native_dispatcher",
                          "compatible_profile_commands", "verify_execution_plan", "verify_envelope"]
    metadata_source = insert_body(sources[runner], "verify_envelope")
    companion = "e2e/local-acceptance/resource_scheduling_acceptance.py"
    changed_companion = sources[companion] + "\n# Private metadata enforcement input.\n"
    metadata_pins = sources[profiles]
    for name, contents in [(runner, metadata_source), (companion, changed_companion)]:
        metadata_pins, count = re.subn(r'("' + re.escape(name) + r'": ")[0-9a-f]{64}(")',
            lambda match: match[1] + hashlib.sha256(contents.encode()).hexdigest() + match[2], metadata_pins)
        assert count == 1, name
    declaration = next(node for node in ast.parse(sources[runner]).body
                       if isinstance(node, ast.FunctionDef) and node.name == "verify_envelope")
    missing = sources[runner].splitlines(keepends=True)
    del missing[declaration.lineno - 1:declaration.end_lineno]
    forged = insert_body(sources[runner], "reachable_profile_commands_hashes",
                         "return {name: 'forged-fingerprint' for name in names}")
    all_bodies = sources[runner]
    for name in metadata_functions:
        all_bodies = insert_body(all_bodies, name)
    metadata_cases = [("single-verifier", {runner: metadata_source}, True),
                      ("all-verifier-bodies", {runner: all_bodies}, True)]
    metadata_cases += [
        ("fixture-and-pins", {runner: metadata_source, profiles: metadata_pins, companion: changed_companion}, True),
        ("process-executor", {runner: insert_body(metadata_source, "run_command")}, False),
        ("selector-function", {runner: insert_body(metadata_source, "selected")}, False),
        ("module-global", {runner: metadata_source + "\nMETADATA_SCOPE_STATE = True\n"}, False),
        ("signature", {runner: metadata_source.replace("completed_only=False):", "completed_only=True):")}, False),
        ("decorator", {runner: metadata_source.replace("\ndef verify_envelope(", "\n@((lambda function: function))\ndef verify_envelope(")}, False),
        ("missing-function", {runner: "".join(missing)}, False),
        ("duplicate-function", {runner: metadata_source + "\ndef verify_envelope(*args):\n    return None\n"}, False),
        ("new-helper", {runner: metadata_source + "\ndef unknown_metadata_helper():\n    pass\n"}, False),
        ("forged-fingerprint-and-command", {runner: forged, profiles: insert_body(sources[profiles], "run")}, False),
        ("profile-command", {runner: metadata_source, profiles: insert_body(sources[profiles], "run")}, False),
        ("policy", {runner: metadata_source, "spec/local_acceptance.json": json.dumps({"version": 1, "repository": "changed", "checks": {}})}, False),
        ("signers", {runner: metadata_source, "spec/local_acceptance_signers.json": '{"version":1,"signers":{}}'}, False),
        ("unknown-input", {runner: metadata_source, ".github/unknown-metadata.py": "# Unknown input\n"}, False),
        ("product", {runner: metadata_source, "rust/crates/runtime/src/metadata_scope.rs": "// Product input\n"}, False),
        ("workflow", {runner: metadata_source, ".github/workflows/rust-ci.yml": "# Changed workflow\n"}, False),
    ]
    for label, edits, expected in metadata_cases:
        git("switch", "--detach", base)
        for name, contents in edits.items():
            write(name, contents)
        git("add", ".")
        git("commit", "-m", "Verifier metadata scope " + label)
        observe("metadata-" + label, base, git("rev-parse", "HEAD"), expected,
                kind="metadata", rust=label == "product", clippy=label in {"product", "workflow"},
                events=None if label in {"single-verifier", "forged-fingerprint-and-command"} else ["merge_group"])
    for label, body in [("static-observer", "return verify_envelope"),
                        ("dynamic-observer", 'return globals().get("verify_" + "envelope")'),
                        ("frame-observer", "return RUNNER_CODE")]:
        git("switch", "--detach", base)
        observer = sources[runner] + "\ndef metadata_observer():\n    " + body + "\n"
        observer = insert_body(observer, "run_command", "metadata_observer()")
        write(runner, observer)
        git("add", ".")
        git("commit", "-m", "Existing queue observer " + label)
        before = git("rev-parse", "HEAD")
        write(runner, insert_body(observer, "verify_envelope"))
        git("add", ".")
        git("commit", "-m", "Metadata edit observed by queue consumer " + label)
        observe("metadata-" + label, before, git("rev-parse", "HEAD"), False, kind="metadata", events=["merge_group"])
    # Import-time observers can affect actual command environments even when
    # run_command itself and all metadata declarations compare equal.
    def verifier_value(source, value):
        node = next(item for item in ast.parse(source).body
                    if isinstance(item, ast.FunctionDef) and item.name == "verify_envelope")
        lines = source.splitlines(keepends=True)
        return "".join(lines[:node.body[0].lineno - 1]) + "    return " + repr(value) + "\n" + "".join(lines[node.end_lineno:])

    observer_body = (
        '    value = verify_envelope(None, None, None, None)\n'
        '    os.environ["SCODE_METADATA_CANARY"] = value\n'
        '    marker = os.environ.get("SCODE_METADATA_IMPORT_MARKER")\n'
        '    if marker:\n'
        '        Path(marker).write_text(value)\n')
    observers = {
        "module-call": "def metadata_import_observer():\n" + observer_body + "\nmetadata_import_observer()\n",
        "default-call": "def metadata_default_observer():\n" + observer_body + "    return value\n\n"
                        "def metadata_unused(value=metadata_default_observer()):\n    return value\n",
        "decorator-call": "def metadata_decorator_observer(function):\n" + observer_body + "    return function\n\n"
                          "@metadata_decorator_observer\ndef metadata_unused():\n    return None\n",
    }
    imports = []
    mutant_dir = output / "old-behavior-mutant"
    mutant_dir.mkdir()
    selector_source = (SOURCE / ".github/scripts/ci_input_scope.py").read_text(encoding="utf-8")
    assert selector_source.count("if current[3] not in REVIEWED_VERIFIER_LAYOUTS:") == 1
    mutant = selector_source.replace("if current[3] not in REVIEWED_VERIFIER_LAYOUTS:", "if False:", 1)
    (mutant_dir / "ci_input_scope.py").write_text(mutant, encoding="utf-8")
    (mutant_dir / "local_acceptance.py").write_text(sources[runner], encoding="utf-8")
    for label, observer in observers.items():
        git("switch", "--detach", base)
        before_source = verifier_value(sources[runner], "before")
        at = before_source.index('if __name__ == "__main__":')
        before_source = before_source[:at] + observer + "\n" + before_source[at:]
        write(runner, before_source)
        git("add", ".")
        git("commit", "-m", "Existing eager observer " + label)
        first = git("rev-parse", "HEAD")
        write(runner, verifier_value(before_source, "after"))
        git("add", ".")
        git("commit", "-m", "Verifier body observed at import " + label)
        head = git("rev-parse", "HEAD")
        marker = output / (label + ".unexpected-import")
        observe("metadata-import-" + label, first, head, False, kind="metadata", events=["merge_group"],
                overrides={"SCODE_METADATA_IMPORT_MARKER": str(marker)})
        assert not marker.exists(), "Scope proof imported an unreviewed comparison-base constructor"
        event = output / (label + ".mutant-event.json")
        event.write_text(json.dumps({"merge_group": {"base_sha": first, "head_sha": head}}))
        github_output = output / (label + ".mutant-output")
        github_output.touch()
        argv = [sys.executable, str(mutant_dir / "ci_input_scope.py")]
        result = subprocess.run(argv, cwd=repository, env={**environment,
            "GITHUB_EVENT_NAME": "merge_group", "GITHUB_EVENT_PATH": str(event),
            "GITHUB_OUTPUT": str(github_output), "SCODE_METADATA_IMPORT_MARKER": str(marker)}, capture_output=True)
        (output / (label + ".mutant.stdout")).write_bytes(result.stdout)
        (output / (label + ".mutant.stderr")).write_bytes(result.stderr)
        assert result.returncode == 0, result.stderr
        old = json.loads(result.stdout)
        assert old["daemon_required"] == "false", "Old-behavior control must expose the unsafe omission"
        assert marker.read_text() == "before", "Old proof must actually execute the comparison-base constructor"
        values = {}
        for state, ref in [("before", first), ("after", head)]:
            helper_path = output / (label + "-" + state + "-runner.py")
            helper_path.write_text(git("show", f"{ref}:{runner}"), encoding="utf-8")
            log = output / (label + "-" + state + "-native.log")
            loader = ('import importlib.util,sys\nfrom pathlib import Path\n'
                'spec=importlib.util.spec_from_file_location("private_observer",sys.argv[1])\n'
                'm=importlib.util.module_from_spec(spec);spec.loader.exec_module(m);m.ROOT=Path(sys.argv[2])\n'
                'm.run_command([sys.executable,"-c",sys.argv[4]],m.ROOT,Path(sys.argv[3]))\n')
            child = subprocess.run([sys.executable, "-c", loader, str(helper_path), str(repository), str(log),
                'import os;print(os.environ["SCODE_METADATA_CANARY"])'],
                cwd=repository, env={key: value for key, value in environment.items()
                                     if key != "SCODE_METADATA_IMPORT_MARKER"}, capture_output=True)
            (output / (label + "-" + state + ".stdout")).write_bytes(child.stdout)
            (output / (label + "-" + state + ".stderr")).write_bytes(child.stderr)
            assert child.returncode == 0, child.stderr
            values[state] = log.read_text().strip()
        assert values == {"before": "before", "after": "after"}, values
        imports.append({"case": label, "base": first, "head": head, "new_scope_refused_without_import": True,
                        "old_behavior_mutant_unsafe_skip": True, "actual_native_child_values": values})
    assert not git("status", "--porcelain"), "Private source/index changed after mode-only controls"
    result = {"status": "PASS", "cases": records, "git_commands": commands,
              "targeted_metadata_only": metadata_only,
              "actual_import_observer_controls": imports,
              "scope": "Actual Git commits and original PR/push/queue selector CLI. No Cargo, daemon, provider or product coverage."}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(f"PASS: {len(records)} real Git/event scheduling/verifier scope and conservative execution/mode boundaries", flush=True)
    return result


def policy_fixture_scope(output, environment):
    """Exercise the actual selector against real committed policy inputs."""
    output.mkdir(parents=True)
    repository = output / "r"
    repository.mkdir()
    environment = {key: value for key, value in environment.items()
                   if not key.upper().startswith(("GIT_", "GITHUB_"))
                   and key.upper() not in {"GH_TOKEN", "GITHUB_TOKEN"}}
    environment.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1",
                       PYTHONDONTWRITEBYTECODE="1", PYTHONIOENCODING="utf-8")
    if os.name == "nt":
        bash = git_for_windows_bash(environment.get("PATH"))
        if not bash:
            raise RuntimeError("The real scanner control requires Git for Windows Bash")
        environment["PATH"] = str(Path(bash).parent) + os.pathsep + environment["PATH"]
    commands, cases = [], []

    def command(label, argv, *, expected=0, env=None):
        result = subprocess.run(list(map(str, argv)), cwd=repository,
                                env=env or environment, capture_output=True)
        prefix = output / f"{len(commands):03}-{label}"
        prefix.with_suffix(".stdout").write_bytes(result.stdout)
        prefix.with_suffix(".stderr").write_bytes(result.stderr)
        commands.append({"label": label, "argv": list(map(str, argv)),
                         "exit_code": result.returncode,
                         "stdout_sha256": hashlib.sha256(result.stdout).hexdigest(),
                         "stderr_sha256": hashlib.sha256(result.stderr).hexdigest()})
        assert (result.returncode == 0) == (expected == 0), (label, result.stdout, result.stderr)
        return result

    def git(*args):
        return command("git", ["git", *args]).stdout.decode().strip()

    selector = ".github/scripts/ci_input_scope.py"
    text = (SOURCE / selector).read_text(encoding="utf-8")
    tree = ast.parse(text)
    registry_node = next(node for node in tree.body if isinstance(node, ast.Assign)
                         and any(isinstance(target, ast.Name) and target.id == "POLICY_FIXTURE_INPUTS"
                                 for target in node.targets))
    registry = ast.literal_eval(registry_node.value)
    sources = {name: (SOURCE / name).read_text(encoding="utf-8")
               for name in {*registry, selector, "spec/local_acceptance.json", "rust/Cargo.toml", "rust/Cargo.lock",
                            ".github/local-acceptance/checks.py", ".github/local-acceptance/Dockerfile"}}

    def write(name, contents):
        path = repository / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents, encoding="utf-8", newline="\n")

    git("init", "--initial-branch=main")
    git("config", "user.name", "Preflight policy scope acceptance")
    git("config", "user.email", "policy-scope@example.invalid")
    git("config", "core.autocrlf", "false")
    git("config", "core.filemode", "false")
    git("config", "core.symlinks", "false")
    for name, contents in sources.items():
        write(name, contents)
    write("rust/crates/runtime/src/policy_scope.rs", "pub fn value() -> u8 { 1 }\n")
    write("docs/policy-scope.md", "Original documentation.\n")
    git("add", ".")
    for name, (mode, _kind, _identity) in registry.items():
        git("update-index", "--chmod=" + ("+x" if mode == "100755" else "-x"), name)
    git("commit", "-m", "Reviewed policy programs and ordinary source")
    base = git("rev-parse", "HEAD")

    def commit(name, contents):
        git("switch", "--detach", base)
        write(name, contents)
        git("add", "--", name)
        git("commit", "-m", "Private policy scope input")
        return git("rev-parse", "HEAD")

    # Exercise the committed registration predicates independently of the
    # workflow event. Only the complete guard grants acceptance credit.
    validate = '''import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
import local_acceptance
from ci_input_scope import check_guard_registrations
local_acceptance.ROOT = Path.cwd()
check_guard_registrations(sys.argv[2])
'''

    def amended_registration(name, key):
        text = sources[name]
        declaration, = [node for node in ast.walk(ast.parse(text)) if isinstance(node, ast.Assign)
                        and any(isinstance(target, ast.Name) and target.id == key for target in node.targets)]
        original = ast.get_source_segment(text, declaration.value)
        assert original and text.count(original) == 1
        return text.replace(original, repr({"0" * 64}), 1)

    runner = ".github/scripts/local_acceptance.py"
    fixture_input = "scripts/publish_release_mirror.py"
    controls = [("valid-registrations", base, None),
                ("unreviewed-resource-layout", commit(runner, amended_registration(runner, "reviewed_sources")),
                 "Unreviewed complete executor program"),
                ("stale-policy-input", commit(fixture_input, sources[fixture_input] + "\n# Changed input\n"),
                 "Unreviewed policy-fixture input: " + fixture_input),
                ("stale-verifier-layout", commit(selector, amended_registration(selector, "REVIEWED_VERIFIER_LAYOUTS")),
                 "Verifier scope registration failed")]
    for label, ref, diagnostic in controls:
        git("switch", "--detach", ref)
        result = command(label, [sys.executable, "-I", "-B", "-c", validate, repository / ".github/scripts", ref],
                         expected=1 if diagnostic else 0)
        if diagnostic:
            assert diagnostic.encode() in result.stderr, (label, result.stderr)

    def observe(label, first, head, expected, *, checkout=None, events=None, declared=None):
        git("switch", "--detach", checkout or head)
        actual_checkout = git("rev-parse", "HEAD")
        observations = []
        for event in events or ["pull_request", "push", "merge_group"]:
            body = ({"pull_request": {"base": {"sha": first}, "head": {"sha": head}}}
                    if event == "pull_request" else {"before": first, "after": head}
                    if event == "push" else {"merge_group": {"base_sha": first, "head_sha": head}}
                    if event == "merge_group" else {})
            event_file = output / f"{label}-{event}.event.json"
            result_file = output / f"{label}-{event}.github-output"
            event_file.write_text(json.dumps(body), encoding="utf-8")
            result_file.write_text("", encoding="utf-8")
            result = command(label, [sys.executable, repository / selector], env={
                **environment, "GITHUB_EVENT_NAME": event, "GITHUB_EVENT_PATH": str(event_file),
                "GITHUB_OUTPUT": str(result_file), "GITHUB_SHA": declared or actual_checkout})
            data = json.loads(result.stdout)
            assert data["policy_fixtures_required"] == str(expected).lower(), (label, data)
            emitted = dict(line.split("=", 1) for line in result_file.read_text().splitlines())
            assert emitted["policy_fixtures_required"] == data["policy_fixtures_required"]
            if label in {"ordinary-rust-source", "manual"}:
                assert all(data[key] == "true" for key in ["live_required", "daemon_required",
                           "a2a_required", "cohost_required", "clippy_required"]), data
            if label == "documentation":
                assert all(data[key] == "false" for key in ["live_required", "daemon_required",
                           "a2a_required", "cohost_required", "clippy_required"]), data
            if not expected:
                assert data["policy_fixture_scope"]["checkout"] == actual_checkout
                assert data["policy_fixture_scope"]["reviewed_input_count"] == len(registry)
            observations.append(data)
            cases.append({"case": label, "event": event, "required": expected,
                          "actual_checkout": actual_checkout, "observation": data})
        return observations

    ordinary = commit("rust/crates/runtime/src/policy_scope.rs", "pub fn value() -> u8 { 2 }\n")
    observe("ordinary-rust-source", base, ordinary, False)
    docs = commit("docs/policy-scope.md", "Changed documentation.\n")
    observe("documentation", base, docs, False)
    for label, name, contents in [
            ("test-source", "rust/crates/runtime/tests/policy_scope.rs", "// Required test input.\n"),
            ("configuration", "rust/Cargo.toml", sources["rust/Cargo.toml"] + "\n# Changed configuration input.\n"),
            ("unknown-input", "tools/unknown-policy.py", "pass\n"),
            ("direct-policy-input", "scripts/publish_release_mirror.py",
             sources["scripts/publish_release_mirror.py"] + "\n# Changed policy input.\n")]:
        changed = commit(name, contents)
        observe(label, base, changed, True)
    observe("manual", base, ordinary, True, events=["workflow_dispatch"])
    observe("empty-change", base, base, True)
    observe("declared-source-mismatch", base, ordinary, True, declared=base, events=["pull_request"])
    git("switch", "--detach", base)
    git("rm", "--", "docs/policy-scope.md")
    git("commit", "-m", "Private deleted documentation input")
    observe("deleted-source", base, git("rev-parse", "HEAD"), True)
    git("switch", "--detach", base)
    source_path = "rust/crates/runtime/src/policy_scope.rs"
    write(source_path, "synthetic-policy-link-target")
    git("add", "--", source_path)
    blob = git("rev-parse", ":" + source_path)
    git("update-index", "--cacheinfo", "120000," + blob + "," + source_path)
    git("commit", "-m", "Private symbolic source input without a filesystem link")
    observe("symbolic-source", base, git("rev-parse", "HEAD"), True)
    git("switch", "--detach", base)
    git("update-index", "--chmod=-x", ".github/scripts/check_engine_boundary.sh")
    git("commit", "-m", "Private policy input mode change")
    changed_mode = git("rev-parse", "HEAD")
    write("docs/policy-scope.md", "Documentation after mode-only policy change.\n")
    git("add", "docs/policy-scope.md")
    git("commit", "-m", "Ordinary input under unreviewed policy mode")
    observe("inherited-policy-mode", changed_mode, git("rev-parse", "HEAD"), True)

    # A new comparison base must not silently register an unreviewed checker.
    scanner = ".github/scripts/check_engine_boundary.sh"
    changed_checker = commit(scanner, "#!/usr/bin/env bash\nexit 0\n")
    write("docs/policy-scope.md", "Documentation after a changed checker.\n")
    git("add", "docs/policy-scope.md")
    git("commit", "-m", "Ordinary source after unreviewed policy change")
    newer = git("rev-parse", "HEAD")
    observe("unreviewed-inherited-program", changed_checker, newer, True)
    failed = command("actual-broken-scanner-self-test",
                     [sys.executable, repository / "e2e/engine-boundary/run.py"], expected=1)
    assert failed.returncode != 0
    assert b"PASS: engine internals are allowed below the seam" in failed.stdout
    assert b"renderer rejects runtime::RuntimeObserver" in failed.stderr
    # Removing the reviewed-input comparison is an actual unsafe selector mutant.
    unsafe = text.replace("if observed.get(path) != expected:", "if False:", 1)
    assert unsafe != text
    write(selector, unsafe)
    git("add", selector)
    git("commit", "-m", "Private missing-registry-check counterexample")
    unsafe_base = git("rev-parse", "HEAD")
    write("docs/policy-scope.md", "Documentation after unsafe selector.\n")
    git("add", "docs/policy-scope.md")
    git("commit", "-m", "Ordinary source under unsafe selector")
    unsafe_head = git("rev-parse", "HEAD")
    observe("missing-registry-control", unsafe_base, unsafe_head, False)

    # The running PR workflow may be a combined checkout newer than its PR head.
    git("switch", "--detach", changed_checker)
    git("merge", "--no-ff", docs, "-m", "Private combined workflow checkout")
    combined = git("rev-parse", "HEAD")
    observe("combined-checkout", base, docs, True, checkout=combined, events=["pull_request"])
    git("switch", "--detach", ordinary)
    write(scanner, sources[scanner] + "\n# Uncommitted checker input.\n")
    observe("dirty-checkout", base, ordinary, True, events=["pull_request"])
    git("restore", "--", scanner)
    assert not git("status", "--porcelain")

    workflow = (SOURCE / ".github/workflows/rust-ci.yml").read_text(encoding="utf-8")
    lines = workflow.splitlines()
    guarded = ["Exercise real Git histories", "Exercise queue cancellation completion races",
               "Exercise real Git boundary violations and scanner failures",
               "Exercise interrupted publication and real installer", "Exercise provider probe contracts",
               "Exercise explicit model selection and shared native runner without a key",
               "Exercise exact published pin tags and refusal paths"]
    for name in guarded:
        start = lines.index("      - name: " + name)
        assert lines[start + 1] == "        if: steps.scope.outputs.policy_fixtures_required == 'true'", name
    for name in ["Reject silent merge rewrites and conflict markers",
                 "Renderer crates must not name engine-internal streaming types",
                 "Canonical Nexus contract and daemon resolve one published pin"]:
        start = lines.index("      - name: " + name)
        end = next((i for i in range(start + 1, len(lines)) if lines[i].startswith("      - ")), len(lines))
        assert "policy_fixtures_required" not in "\n".join(lines[start:end]), name
    assert lines.index("      - name: Canonical Nexus contract and daemon resolve one published pin") < lines.index(
        "      - name: Exercise real Git histories")
    result = {"status": "PASS", "cases": cases, "commands": commands,
              "committed_registration_controls": [{"case": label, "accepted": diagnostic is None}
                                                   for label, _, diagnostic in controls],
              "actual_broken_scanner_fixture_refused": True,
              "unsafe_selector_omits_that_required_fixture": True,
              "original_actual_current_source_checks_ungated": True,
              "scope": "Real Git/PR/push/queue selector and actual scanner refusal; no product or population latency acceptance."}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(f"PASS: {len(cases)} real preflight policy-fixture input/event boundaries", flush=True)
    return result


def run(output):
    output.mkdir(parents=True, exist_ok=False)
    repository = output / "repository"
    repository.mkdir()
    environment = {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}
    environment.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM="1",
                       CARGO_BUILD_JOBS="1", CARGO_PROFILE_DEV_DEBUG="0",
                       CARGO_PROFILE_TEST_DEBUG="0", CARGO_INCREMENTAL="0")
    if os.name == "nt":
        # Identical Windows links otherwise change timestamps/PDB GUIDs.
        # Make this private canary reproducible; product flags stay untouched.
        if "CARGO_ENCODED_RUSTFLAGS" in environment:
            environment["CARGO_ENCODED_RUSTFLAGS"] += "\x1f-C\x1flink-arg=/Brepro"
        else:
            environment["RUSTFLAGS"] = environment.get("RUSTFLAGS", "") + " -C link-arg=/Brepro"
    policy_fixtures = policy_fixture_scope(output / "policy-fixture-scope", environment)
    commands, cases = [], []

    def command(*args, success=True):
        result = subprocess.run(list(map(str, args)), cwd=repository, env=environment,
                                capture_output=True, text=True, encoding="utf-8", errors="replace")
        if (result.returncode == 0) != success:
            raise AssertionError(str(args) + "\n" + result.stdout + result.stderr)
        return result.stdout.strip() if success else result.stdout + result.stderr

    def write(path, text):
        destination = repository / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(text, encoding="utf-8")

    def commit(subject):
        command("git", "add", ".")
        command("git", "commit", "-m", subject)
        return command("git", "rev-parse", "HEAD")

    command("git", "init", "--initial-branch=main")
    command("git", "config", "user.name", "Dependency scope acceptance")
    command("git", "config", "user.email", "scope@example.invalid")
    command("git", "config", "core.autocrlf", "false")
    actual_metadata = json.loads(command("cargo", "metadata", "--no-deps", "--locked", "--format-version=1",
                                         "--manifest-path", SOURCE / "rust/Cargo.toml"))
    actual_cli = next(package for package in actual_metadata["packages"] if package["name"] == "rusty-sudocode-cli")
    assert not any(set(target["kind"]) & {"lib", "rlib", "dylib", "cdylib", "staticlib", "proc-macro"}
                   for target in actual_cli["targets"]), "The CLI gained a shared library; revisit co-host scope"
    cli_directory = Path(actual_cli["manifest_path"]).parent.resolve()
    assert not any(dependency["name"] == "rusty-sudocode-cli"
                   or (dependency.get("path") and Path(dependency["path"]).resolve() == cli_directory)
                   for package in actual_metadata["packages"] if package["id"] != actual_cli["id"]
                   for dependency in package["dependencies"]), "A workspace package now depends on the CLI"
    configuration = subprocess.run(["git", "config", "--get", "localAcceptance.vsdevcmd"],
                                   cwd=SOURCE, env=environment, capture_output=True, text=True).stdout.strip()
    if configuration:
        command("git", "config", "localAcceptance.vsdevcmd", configuration)
    for name in [".github/scripts/local_acceptance.py", ".github/scripts/ci_input_scope.py", "spec/local_acceptance.json"]:
        write(name, (SOURCE / name).read_text(encoding="utf-8"))
    write(".gitignore", "rust/target/\n")
    workspace = '[workspace]\nmembers = ["crates/*"]\nresolver = "2"\n'
    application = ('[package]\nname="rusty-sudocode-cli"\nversion="0.1.0"\nedition="2021"\n'
                   '[[bin]]\nname="scope-product"\npath="src/main.rs"\n'
                   '[dependencies]\nruntime={path="../runtime"}\n')
    write("rust/Cargo.toml", workspace)
    write("rust/crates/rusty-sudocode-cli/Cargo.toml", application)
    write("rust/crates/rusty-sudocode-cli/src/main.rs", 'fn main() { println!("product-{}", runtime::value()); }\n')
    write("rust/crates/rusty-sudocode-cli/tests/development.rs", '#[test] fn native_case() { assert_eq!(runtime::value(), 42); }\n')
    write("rust/crates/runtime/Cargo.toml", '[package]\nname="runtime"\nversion="0.1.0"\nedition="2021"\n[features]\ntest-helper=[]\n')
    write("rust/crates/runtime/src/lib.rs", '#[cfg(not(feature="test-helper"))] pub fn value() -> u8 { 42 }\n'
          '#[cfg(feature="test-helper")] pub fn value() -> u8 { 99 }\n')
    write("rust/crates/helper/Cargo.toml", '[package]\nname="scope-helper"\nversion="0.1.0"\nedition="2021"\n'
          '[dependencies]\nruntime={path="../runtime",features=["test-helper"]}\n')
    write("rust/crates/helper/src/lib.rs", 'pub fn value() -> u8 { runtime::value() }\n')
    write("rust/crates/cohost/Cargo.toml", '[package]\nname="scope-cohost"\nversion="0.1.0"\nedition="2021"\n'
          '[dependencies]\nruntime={path="../runtime"}\n')
    write("rust/crates/cohost/src/main.rs", 'fn main() { println!("cohost-{}", runtime::value()); }\n')
    command("cargo", "generate-lockfile", "--offline", "--manifest-path", "rust/Cargo.toml")
    base = commit("Baseline native product and unused development helper")
    helper = SOURCE / ".github/scripts/local_acceptance.py"
    spec = importlib.util.spec_from_file_location("dependency_scope_executor", helper)
    executor = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(executor)
    executor.ROOT = repository
    target = repository / "rust/target"
    build_environment = {**environment, "CARGO_TARGET_DIR": str(target)}

    def cargo(*args):
        result = executor.run_command(["cargo", *args], repository / "rust",
                                      output / f"cargo-{len(commands):02}.log", build_environment)
        commands.append(result)
        return result

    def product():
        cargo("build", "--locked", "--offline", "-p", "rusty-sudocode-cli")
        binary = target / "debug" / ("scope-product.exe" if os.name == "nt" else "scope-product")
        assert command(binary) == "product-42"
        return hashlib.sha256(binary.read_bytes()).hexdigest()

    original_binary = product()

    def cohost(value=42):
        cargo("build", "--locked", "--offline", "-p", "scope-cohost")
        binary = target / "debug" / ("scope-cohost.exe" if os.name == "nt" else "scope-cohost")
        assert command(binary) == f"cohost-{value}"
        return hashlib.sha256(binary.read_bytes()).hexdigest()

    original_cohost = cohost()
    workflow = workflow_scope(output / "workflow-scope", environment)
    scheduling = scheduling_scope(output / "scheduling-scope", environment)
    assert scheduling["targeted_metadata_only"] is False, "Required fixture must execute all original scheduling controls"
    development = application + '[dev-dependencies]\nscope-helper={path="../helper"}\n'
    write("rust/crates/rusty-sudocode-cli/Cargo.toml", development)
    write("rust/crates/rusty-sudocode-cli/tests/development.rs", '#[test] fn native_case() { assert_eq!(scope_helper::value(), 99); }\n')
    command("cargo", "generate-lockfile", "--offline", "--manifest-path", "rust/Cargo.toml")
    changed = commit("Add development dependency and actual native helper case")

    def expect(label, diff_base, expected):
        text = command(sys.executable, helper, "plan", "--base", diff_base, "--policy-ref", "main",
                       "--host", "windows", "--architecture", "AMD64")
        plan = json.loads(next(line.split("=", 1)[1] for line in text.splitlines() if line.startswith("ACCEPTANCE_PLAN=")))
        names = set(plan["local_checks"])
        assert names & PRODUCT == set(expected), (label, text)
        assert "native-workspace" in names, (label, text)
        # Ordinary dependency metadata omits performance; actual CLI/driver
        # changes require local rendering and the functional browser smoke.
        changed = command("git", "diff", "--name-only", "--no-renames", diff_base, "HEAD").splitlines()
        smoke = {"rendering", "browser"} if any(path.startswith(("rust/crates/rusty-sudocode-cli/src/",
                                                    "rust/crates/scode-benchmark-drivers/"))
                                   for path in changed) else set()
        assert {"rendering", "browser"} & names == smoke, (label, text)
        cases.append({"label": label, "base": diff_base, "head": command("git", "rev-parse", "HEAD"),
                      "product_checks": sorted(names & PRODUCT), "native_tests_retained": True})
        (output / "cases.json").write_text(json.dumps(cases, indent=2) + "\n")
        print("PASS: " + label, flush=True)

    def ci_scope(diff_base, daemon, benchmark=False, cohost_required=None, live_required=True,
                 clippy_required=True):
        cohost_required = daemon if cohost_required is None else cohost_required
        head = command("git", "rev-parse", "HEAD")
        keys = ["GITHUB_EVENT_NAME", "GITHUB_EVENT_PATH", "GITHUB_OUTPUT"]
        previous = {key: environment.get(key) for key in keys}
        try:
            for event, data in [
                ("pull_request", {"pull_request": {"base": {"sha": diff_base}, "head": {"sha": head}}}),
                ("push", {"before": diff_base, "after": head}),
                ("merge_group", {"merge_group": {"base_sha": diff_base, "head_sha": head}}),
            ]:
                event_path = output / "ci-event.json"
                event_path.write_text(json.dumps(data))
                result_path = output / "ci-output.txt"
                result_path.write_text("")
                environment.update(GITHUB_EVENT_NAME=event, GITHUB_EVENT_PATH=str(event_path),
                                   GITHUB_OUTPUT=str(result_path))
                result = json.loads(command(sys.executable, repository / ".github/scripts/ci_input_scope.py"))
                assert result["daemon_required"] == str(daemon).lower(), result
                assert result["a2a_required"] == str(daemon).lower(), result
                assert result["cohost_required"] == str(cohost_required).lower(), result
                emitted = dict(line.split("=", 1) for line in result_path.read_text().splitlines())
                assert emitted["cohost_required"] == result["cohost_required"], emitted
                if clippy_required:
                    assert result["clippy_required"] == "true", result
                else:
                    assert result["clippy_required"] == "false", result
                assert result["live_required"] == str(live_required).lower(), result
                assert result["benchmark_required"] == str(benchmark).lower(), result
                print(f"PASS: actual {event} selector daemon={daemon}, cohost={cohost_required}, lint={clippy_required}, live={live_required}", flush=True)
        finally:
            for key, value in previous.items():
                if value is None:
                    environment.pop(key, None)
                else:
                    environment[key] = value

    expect("dev-only manifest/lock skips unchanged normal products", base, [])
    ci_scope(base, False)
    queue_base = command("git", "rev-parse", "HEAD")
    queue_branch = command("git", "branch", "--show-current")
    command("git", "switch", "--detach", queue_base)
    for path, product_required, benchmark_required in [
            (".github/workflows/rust-ci.yml", True, True),
            (".github/scripts/ci_input_scope.py", False, True),
            (".github/actions/rust-toolchain/cache-command.mjs", True, False)]:
        original = (repository / path).read_text() if (repository / path).exists() else ""
        write(path, original + "\n# Changed queue execution input.\n")
        commit("Update queue workflow or selection script")
        ci_scope(queue_base, product_required, benchmark=benchmark_required, live_required=False,
                 clippy_required=product_required)
        command("git", "switch", "--detach", queue_base)
    command("git", "switch", queue_branch)
    assert product() == original_binary
    cargo("test", "--locked", "--offline", "-p", "rusty-sudocode-cli", "--test", "development")
    assert product() == original_binary
    positive_lock = (repository / "rust/Cargo.lock").read_text()

    # Update a real shared Git dependency, including a package-version change.
    # The same public planner/CI entry points must retain the native driver
    # while omitting only unchanged normal products. Normal consumers below
    # deliberately compile broken Git code, so stale product binaries cannot
    # turn a required normal build into a passing control.
    positive_state = command("git", "rev-parse", "HEAD")
    previous_offline = environment.get("CARGO_NET_OFFLINE")
    environment["CARGO_NET_OFFLINE"] = "false"  # Fetch only this fixture's local file:// Git source.
    shared = output / "shared-git-dependency"
    shared.mkdir()
    command("git", "init", "--initial-branch=main", shared)
    command("git", "-C", shared, "config", "user.name", "Dependency scope acceptance")
    command("git", "-C", shared, "config", "user.email", "scope@example.invalid")
    command("git", "-C", shared, "config", "core.autocrlf", "false")
    (shared / "src").mkdir()

    def revision(value, version="0.1.0", broken=False):
        (shared / "Cargo.toml").write_text('[package]\nname="scope-shared-helper"\n'
            f'version="{version}"\nedition="2021"\n', encoding="utf-8")
        (shared / "src/lib.rs").write_text(
            ('compile_error!("shared-normal-refusal");\n' if broken else "")
            + f'pub fn value() -> u8 {{ {value} }}\n', encoding="utf-8")
        command("git", "-C", shared, "add", ".")
        command("git", "-C", shared, "commit", "-m", f"Native dependency value {value}")
        return command("git", "-C", shared, "rev-parse", "HEAD")

    first_revision = revision(7)
    shared_application = development + 'scope-git={workspace=true}\n'

    def shared_workspace(ref):
        return workspace + '[workspace.dependencies]\nscope-git={package="scope-shared-helper",git="' + shared.as_uri() + '",rev="' + ref + '"}\n'

    def shared_driver(value):
        write("rust/crates/rusty-sudocode-cli/tests/development.rs",
            '#[test] fn native_case() { assert_eq!(scope_helper::value(), 99); '
            f'assert_eq!(scope_git::value(), {value}); }}\n')

    write("rust/Cargo.toml", shared_workspace(first_revision))
    write("rust/crates/rusty-sudocode-cli/Cargo.toml", shared_application)
    shared_driver(7)
    # Only local file:// Git repositories are involved; no registry/model API.
    command("cargo", "generate-lockfile", "--manifest-path", "rust/Cargo.toml")
    shared_base = commit("Add a shared aliased Git development dependency")
    expect("shared workspace dev Git addition skips normal products", positive_state, [])
    ci_scope(positive_state, False)
    assert product() == original_binary and cohost() == original_cohost
    cargo("test", "--locked", "--offline", "-p", "rusty-sudocode-cli", "--test", "development")
    shared_revisions = []
    for value, version in [(8, "0.1.0"), (9, "0.2.0")]:
        ref = revision(value, version)
        write("rust/Cargo.toml", shared_workspace(ref))
        shared_driver(value)
        command("cargo", "generate-lockfile", "--manifest-path", "rust/Cargo.toml")
        commit("Update shared development Git source and actual driver expectation")
        expect(f"shared dev Git value {value} version {version} keeps normal products unchanged", shared_base, [])
        ci_scope(shared_base, False)
        assert product() == original_binary and cohost() == original_cohost
        cargo("test", "--locked", "--offline", "-p", "rusty-sudocode-cli", "--test", "development")
        shared_revisions.append(ref)
    stable_revision = shared_revisions[-1]
    current_target = "cfg(windows)" if os.name == "nt" else "cfg(unix)"
    write("rust/crates/rusty-sudocode-cli/Cargo.toml", development
          + f'[target.\'{current_target}\'.dev-dependencies]\nscope-git={{workspace=true}}\n')
    command("cargo", "generate-lockfile", "--manifest-path", "rust/Cargo.toml")
    commit("Use the shared dependency through a target-specific development alias")
    expect("target-specific shared dev aliases retain native tests and skip normal products", shared_base, [])
    ci_scope(shared_base, False)
    assert product() == original_binary and cohost() == original_cohost
    cargo("test", "--locked", "--offline", "-p", "rusty-sudocode-cli", "--test", "development")

    # Keep a normal transitive Git branch declaration fixed while its actual
    # lock resolution changes alongside the development revision.
    runtime_manifest = (repository / "rust/crates/runtime/Cargo.toml").read_text()
    write("rust/crates/runtime/Cargo.toml", runtime_manifest
          + '[dependencies]\nnormal-alias={package="scope-shared-helper",git="'
          + shared.as_uri() + '",branch="main"}\n')
    command("cargo", "generate-lockfile", "--manifest-path", "rust/Cargo.toml")
    transitive_base = commit("Normal runtime imports a transitive Git branch")
    product()
    broken_revision = revision(10, "0.2.0", broken=True)
    write("rust/Cargo.toml", shared_workspace(broken_revision))
    command("cargo", "generate-lockfile", "--manifest-path", "rust/Cargo.toml")
    commit("Actual normal Git branch and dev revision both resolve broken code")
    expect("normal transitive resolution change retains every normal gate", transitive_base, PRODUCT)
    ci_scope(transitive_base, True)

    def normal_git_refusal(label):
        log = output / f"{label}.log"
        try:
            executor.run_command(["cargo", "build", "--locked", "--offline", "-p", "rusty-sudocode-cli"],
                                 repository / "rust", log, build_environment)
        except RuntimeError:
            refused = json.loads(log.with_suffix(".command.json").read_text())
            commands.append(refused)
            assert refused["exit_code"] != 0 and "shared-normal-refusal" in log.read_text(encoding="utf-8", errors="replace")
        else:
            raise AssertionError("Broken normal Git dependency unexpectedly compiled")

    normal_git_refusal("transitive-normal-refusal")
    write("rust/crates/runtime/Cargo.toml", runtime_manifest)
    for kind in ["dependencies", "build-dependencies", f"target.'{current_target}'.dependencies"]:
        # The normal alias inherits the *same* shared entry when appropriate;
        # separate target/build tables still participate in normal resolution.
        manifest = (shared_application.replace('[dependencies]\n',
                    '[dependencies]\nscope-git={workspace=true}\n', 1) if kind == "dependencies"
                    else shared_application + f'[{kind}]\nscope-git={{workspace=true}}\n')
        write("rust/Cargo.toml", shared_workspace(stable_revision))
        write("rust/crates/rusty-sudocode-cli/Cargo.toml", manifest)
        if kind == "build-dependencies":
            write("rust/crates/rusty-sudocode-cli/build.rs", 'fn main() { assert_eq!(scope_git::value(), 9); }\n')
        command("cargo", "generate-lockfile", "--manifest-path", "rust/Cargo.toml")
        normal_base = commit("Normal alias consumes the shared Git declaration")
        write("rust/Cargo.toml", shared_workspace(broken_revision))
        command("cargo", "generate-lockfile", "--manifest-path", "rust/Cargo.toml")
        commit("Updating a shared normal Git alias keeps required product validation")
        expect(f"shared {kind} Git update retains normal gates", normal_base, PRODUCT)
        ci_scope(normal_base, True)
        normal_git_refusal(kind.replace(".", "-").replace("'", ""))
        if kind == "build-dependencies":
            command("git", "rm", "rust/crates/rusty-sudocode-cli/build.rs")

    # Resume the pre-existing controls from their exact original fixture tree.
    command("git", "restore", "--source", positive_state, "--staged", "--worktree", ".")
    commit("Restore original development fixture after shared Git workflows")
    if previous_offline is None:
        environment.pop("CARGO_NET_OFFLINE", None)
    else:
        environment["CARGO_NET_OFFLINE"] = previous_offline

    # The actual Cargo build must not import a broken, independent CLI source.
    write("rust/crates/rusty-sudocode-cli/src/main.rs", 'compile_error!("unused-CLI-control");\n')
    commit("CLI source keeps daemon journeys without rebuilding the independent co-host")
    expect("CLI source retains normal CLI checks and skips the unchanged co-host", base,
           {"native-context", "native-services"})
    ci_scope(base, True, cohost_required=False)
    assert cohost() == original_cohost
    refused_log = output / "independent-cli-refusal.log"
    try:
        executor.run_command(["cargo", "build", "--locked", "--offline", "-p", "rusty-sudocode-cli"],
                             repository / "rust", refused_log, build_environment)
    except RuntimeError:
        refused_cli = json.loads(refused_log.with_suffix(".command.json").read_text())
        commands.append(refused_cli)
        assert refused_cli["exit_code"] != 0 and "unused-CLI-control" in refused_log.read_text()
    else:
        raise AssertionError("The deliberately broken CLI unexpectedly compiled")
    write("rust/crates/rusty-sudocode-cli/src/lib.rs", 'pub fn shared() {}\n')
    library_head = commit("Automatic CLI library target retains co-host scope")
    expect("a new CLI library target retains conservative co-host scope", base,
           {"native-context", "native-services"})
    ci_scope(base, True)
    command("git", "rm", "rust/crates/rusty-sudocode-cli/src/lib.rs")
    binary_head = commit("Removing a prior CLI library also retains co-host scope")
    expect("a removed CLI library is not mistaken for an unchanged binary boundary", library_head,
           {"native-services"})
    ci_scope(library_head, True)
    cohost_manifest = (repository / "rust/crates/cohost/Cargo.toml").read_text()
    write("rust/crates/cohost/Cargo.toml", cohost_manifest +
          '[build-dependencies]\ncli_runner={package="rusty-sudocode-cli",path="../rusty-sudocode-cli"}\n')
    importer = commit("An unknown declared CLI consumer invalidates independence")
    write("rust/crates/rusty-sudocode-cli/src/main.rs", 'compile_error!("unused-CLI-control");\n// consumer boundary\n')
    commit("CLI source with a declared consumer keeps co-host acceptance")
    expect("a declared CLI consumer retains scope for later CLI source", importer,
           {"native-context", "native-services"})
    ci_scope(importer, True)
    command("git", "switch", "--detach", binary_head)
    write("rust/Cargo.toml", workspace.replace('["crates/*"]',
          '["crates/rusty-sudocode-cli","crates/runtime","crates/helper","crates/cohost"]'))
    layout = commit("A different workspace layout keeps conservative scope")
    write("rust/crates/rusty-sudocode-cli/src/main.rs", 'compile_error!("unused-CLI-control");\n// workspace boundary\n')
    commit("CLI source with an unfamiliar workspace layout keeps co-host acceptance")
    expect("an unfamiliar workspace layout retains scope for later CLI source", layout,
           {"native-context", "native-services"})
    ci_scope(layout, True)
    command("git", "switch", "--detach", binary_head)
    write("rust/crates/runtime/src/lib.rs", 'pub fn value() -> u8 { 43 }\n')
    commit("A mixed CLI and shared runtime change retains co-host acceptance")
    expect("shared runtime source still selects the co-host with CLI source", base, PRODUCT)
    ci_scope(base, True)
    assert cohost(43) != original_cohost
    command("git", "switch", "--detach", changed)

    variants = [
        ("normal dependency features", "rust/crates/rusty-sudocode-cli/Cargo.toml",
         development.replace('runtime={path="../runtime"}', 'runtime={path="../runtime",features=["test-helper"]}'), PRODUCT),
        ("optional normal dependency", "rust/crates/rusty-sudocode-cli/Cargo.toml",
         development.replace('[dependencies]', '[dependencies]\nother={package="scope-helper",path="../helper",optional=true}'), PRODUCT),
        ("build dependency", "rust/crates/rusty-sudocode-cli/Cargo.toml",
         development + '[build-dependencies]\nscope-helper={path="../helper"}\n', PRODUCT),
        ("target normal dependency", "rust/crates/rusty-sudocode-cli/Cargo.toml",
         development + '[target.\'cfg(windows)\'.dependencies]\nother={package="scope-helper",path="../helper"}\n', PRODUCT),
        ("feature definition", "rust/crates/rusty-sudocode-cli/Cargo.toml", development + '[features]\nextra=[]\n', PRODUCT),
        ("workspace resolver change", "rust/Cargo.toml", workspace.replace('"2"', '"1"'), PRODUCT),
        ("unknown workspace setting", "rust/Cargo.toml", workspace + '[profile.dev]\nopt-level=1\n', PRODUCT),
        ("malformed manifest", "rust/crates/rusty-sudocode-cli/Cargo.toml", '[package\n', PRODUCT),
        ("normal lock package changed", "rust/Cargo.lock", positive_lock.replace('name = "runtime"\nversion = "0.1.0"', 'name = "runtime"\nversion = "0.2.0"'), PRODUCT),
        ("ambiguous additional lock version", "rust/Cargo.lock", positive_lock + '\n[[package]]\nname="runtime"\nversion="0.2.0"\n', PRODUCT),
        ("lock format changed", "rust/Cargo.lock", positive_lock.replace('version = 4', 'version = 3'), PRODUCT),
        ("ordinary runtime source keeps services without the context-specific journey", "rust/crates/runtime/src/lib.rs", 'pub fn value() -> u8 { 43 }\n', PRODUCT - {"native-context"}),
        ("CLI build script keeps conservative co-host scope", "rust/crates/rusty-sudocode-cli/build.rs",
         'fn main() { println!("cargo:rerun-if-changed=build.rs"); }\n', {"native-context", "native-services"}),
        ("source stream assertion stays required", "rust/crates/rusty-sudocode-cli/tests/pty_model_stream_close.rs", '// actual changed release fixture\n', {"native-context"}),
        ("service assertion stays required", "e2e/shared-services/scope.py", '# actual changed service fixture\n', {"native-services"}),
        ("target-specific development alias", "rust/crates/rusty-sudocode-cli/Cargo.toml",
         application + '[target.\'cfg(windows)\'.dev-dependencies]\nalias={package="scope-helper",path="../helper"}\n', []),
        ("standalone benchmark driver metadata", "rust/crates/scode-benchmark-drivers/Cargo.toml",
         # This test package is outside the policy's explicit co-host inputs.
         '[package]\nname="scode-benchmark-drivers"\nversion="0.1.0"\nedition="2021"\n', {"native-context", "native-services"}),
    ]
    for index, (label, path, text, expected) in enumerate(variants):
        command("git", "switch", "--detach", changed)
        write(path, text)
        if path.endswith("Cargo.lock"):
            assert text != positive_lock, label
        commit("Scope boundary " + str(index))
        expect(label, base, expected)
        if label in {"ordinary runtime source keeps services without the context-specific journey", "CLI build script keeps conservative co-host scope",
                     "normal dependency features", "build dependency", "normal lock package changed"}:
            ci_scope(base, True)
        elif label == "standalone benchmark driver metadata":
            ci_scope(base, True, benchmark=True)
    command("git", "switch", "--detach", changed)
    command("git", "mv", "rust/crates/runtime/src/lib.rs", "rust/crates/runtime/src/renamed.rs")
    commit("Keep both sides of a renamed source")
    expect("renamed ordinary source keeps services without the context-specific journey", base, PRODUCT - {"native-context"})
    command("git", "switch", "--detach", changed)
    assert not command("git", "status", "--porcelain")
    # An earlier broader receipt/checkpoint must retain actual completed work
    # when this protected selector removes only irrelevant normal builds.
    command("git", "switch", "--detach", base)
    key = output / "signing-key"
    command("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", key)
    policy = {"version": 1, "repository": "acceptance/fixture", "checks": {
        "preflight": {"include": ["**"]},
        "native-workspace": {"include": ["rust/**"]},
        **{name: {"include": ["rust/Cargo.lock", "rust/crates/*/Cargo.toml",
                              "rust/crates/rusty-sudocode-cli/tests/**"]}
           for name in ["native-context", "native-services", "native-cohost"]},
    }}
    write("spec/local_acceptance.json", json.dumps(policy))
    write("spec/local_acceptance_signers.json", json.dumps({"signers": {"fixture": {
        "platform": executor.host()["system"], "public_key": key.with_suffix(".pub").read_text().strip()}}}))
    write(".github/workflows/local-acceptance.yml", "# Private acceptance metadata\n")
    write(".github/workflows/local-acceptance-publish.yml", "# Private publisher metadata\n")
    write(".github/local-acceptance/Dockerfile", "FROM scratch\n")
    write(".github/local-acceptance/checks.py", 'import sys\n'
          'def run(name, root, output, base, execute):\n'
          ' output.mkdir(parents=True)\n'
          ' return [execute([sys.executable, "scope_check.py", name], root, output / "command.log")]\n')
    write("scope_check.py", 'import os,sys\nfrom pathlib import Path\n'
          'with Path(os.environ["SCOPE_CHECK_LOG"]).open("a") as log: log.write(sys.argv[1]+"\\n")\n'
          'print("Actual native profile process: "+sys.argv[1])\n'
          'raise SystemExit(9 if os.environ.get("FAIL_SCOPE_PROFILE")==sys.argv[1] else 0)\n')
    protected = commit("Private signed-scope policy and native process checker")
    command("git", "config", "localAcceptance.signer", "fixture")
    command("git", "config", "localAcceptance.key", key)
    # Use a real earlier broader protected policy, so the regression also
    # runs from Docker's history-free tracked-source snapshot.
    old_helper = helper
    write("rust/crates/rusty-sudocode-cli/Cargo.toml", development)
    write("rust/crates/rusty-sudocode-cli/tests/earlier_scope.rs", "// Earlier policy selects this native input\n")
    command("cargo", "generate-lockfile", "--offline", "--manifest-path", "rust/Cargo.toml")
    full_head = commit("Development-only source for an earlier broader full receipt")
    environment["SCOPE_CHECK_LOG"] = str(output / "full-profile-count.txt")
    old_full = output / "old-full"
    command(sys.executable, old_helper, "check", "--base", protected, "--policy-ref", protected, "--output", old_full)
    assert len(Path(environment["SCOPE_CHECK_LOG"]).read_text().splitlines()) == 5
    note = output / "actual-signed-note.json"
    note.write_text(json.dumps({"version": 1, "attestations": {
        "fixture": json.loads((old_full / "envelope.json").read_text())}}))
    command("git", "notes", "--ref=refs/notes/local-acceptance", "add", "-F", note, full_head)
    command("git", "switch", "--detach", protected)
    for name in PRODUCT:
        policy["checks"][name]["include"].remove("rust/crates/rusty-sudocode-cli/tests/**")
    write("spec/local_acceptance.json", json.dumps(policy))
    narrowed = commit("Protected policy narrows unchanged normal product selection")
    command("git", "switch", "--detach", full_head)
    text = command(sys.executable, helper, "verify", "--head", full_head, "--base", protected, "--policy-ref", narrowed)
    assert "earlier scope" in text, text
    assert len(Path(environment["SCOPE_CHECK_LOG"]).read_text().splitlines()) == 5
    print("PASS: actual earlier full Git/SSH receipt keeps valid execution without rerunning profiles", flush=True)
    legacy = json.loads((old_full / "receipt.json").read_text())
    legacy.pop("plan")
    envelope = executor.sign_receipt(legacy, output / "legacy-without-plan", "fixture", key)
    legacy_note = output / "legacy-signed-note.json"
    legacy_note.write_text(json.dumps({"version": 1, "attestations": {"fixture": envelope}}))
    command("git", "notes", "--ref=refs/notes/local-acceptance", "add", "--force", "-F", legacy_note, full_head)
    command(sys.executable, helper, "verify", "--head", full_head, "--base", protected, "--policy-ref", narrowed)
    assert len(Path(environment["SCOPE_CHECK_LOG"]).read_text().splitlines()) == 5
    print("PASS: actual legacy signed full receipt without a plan retains known completed checks", flush=True)
    # Produce a real later failure and retain its signed earlier progress.
    write("rust/crates/rusty-sudocode-cli/tests/scope_note.rs", "// A distinct unchanged-product commit\n")
    partial_head = commit("Distinct development-only source for actual failed broader attempt")
    environment["SCOPE_CHECK_LOG"] = str(output / "partial-profile-count.txt")
    environment["FAIL_SCOPE_PROFILE"] = "native-cohost"
    old_partial = output / "old-partial"
    command(sys.executable, old_helper, "check", "--base", protected, "--policy-ref", protected,
            "--output", old_partial, success=False)
    assert len(Path(environment["SCOPE_CHECK_LOG"]).read_text().splitlines()) == 5
    resumed = output / "new-resumed"
    command(sys.executable, helper, "check", "--base", protected, "--policy-ref", narrowed,
            "--resume-from", old_partial, "--output", resumed)
    assert len(Path(environment["SCOPE_CHECK_LOG"]).read_text().splitlines()) == 5
    saved = json.loads((resumed / "receipt.json").read_text())
    assert set(saved["checks"]) == {"preflight", "native-workspace"}
    checkpoint = json.loads((old_partial / "attempt.json").read_text())
    assert saved["timing"]["first_recorded_started_at"] == checkpoint["first_recorded_started_at"]
    assert json.loads((old_partial / "native-cohost/command.command.json").read_text())["exit_code"] == 9
    environment.pop("FAIL_SCOPE_PROFILE")
    print("PASS: actual failed broader checkpoint resumes only current checks without repeating completed work or erasing failure/time", flush=True)
    command("git", "switch", "--detach", narrowed)
    policy["checks"]["new-required"] = {"include": ["**"]}
    write("spec/local_acceptance.json", json.dumps(policy))
    expanded = commit("Protected policy adds an actual required check")
    command("git", "switch", "--detach", partial_head)
    failure = command(sys.executable, helper, "verify", "--head", full_head, "--base", protected,
                      "--policy-ref", expanded, success=False)
    assert "new-required" in failure or "Acceptance commands changed" in failure, failure
    print("PASS: the same actual signed full receipt cannot omit a newly required profile", flush=True)
    command("git", "switch", "--detach", protected)
    write("rust/crates/rusty-sudocode-cli/Cargo.toml", development.replace(
        'runtime={path="../runtime"}', 'runtime={path="../runtime",features=["test-helper"]}'))
    command("cargo", "generate-lockfile", "--offline", "--manifest-path", "rust/Cargo.toml")
    commit("Normal feature change retains the actual failing profile")
    environment["SCOPE_CHECK_LOG"] = str(output / "normal-failure-count.txt")
    environment["FAIL_SCOPE_PROFILE"] = "native-cohost"
    normal_partial = output / "normal-old-partial"
    command(sys.executable, old_helper, "check", "--base", protected, "--policy-ref", protected,
            "--output", normal_partial, success=False)
    assert len(Path(environment["SCOPE_CHECK_LOG"]).read_text().splitlines()) == 5
    failure = command(sys.executable, helper, "check", "--base", protected, "--policy-ref", narrowed,
                      "--resume-from", normal_partial, "--output", output / "normal-new-still-fails", success=False)
    assert len(Path(environment["SCOPE_CHECK_LOG"]).read_text().splitlines()) == 6
    assert "Local acceptance failed (9)" in failure, failure
    environment.pop("FAIL_SCOPE_PROFILE")
    print("PASS: an actual normal-input change still reruns and fails its required profile", flush=True)
    # Small Rust or cached builds can finish between process samples. Keep the
    # original observations and require a real, observable compile as well.
    expected_priority = 16384 if os.name == "nt" else 10
    rustc = Path(command("rustc", "--print", "sysroot")) / "bin" / ("rustc.exe" if os.name == "nt" else "rustc")
    commands.append(check_compiler_priority(executor, rustc, output, build_environment, expected_priority))
    # Each row signs {compiler name: {priority: distinct children}}.
    observed = [(priority, count) for row in commands for counts in row.get("compiler_priorities", {}).values()
                for priority, count in counts.items()]
    assert observed and all(priority == str(expected_priority) and count > 0 for priority, count in observed), observed
    result = {"status": "PASS", "cases": cases, "commands": commands,
              "unchanged_normal_binary_sha256": original_binary,
              "actual_development_feature_case_passed": True,
              "shared_workspace_git_revision_and_version_drivers_passed": True,
              "shared_target_dev_alias_keeps_normal_binary_unchanged": True,
              "broken_shared_normal_build_target_and_transitive_Git_dependencies_refused": True,
              "normal_binary_after_development_driver_unchanged": True,
              "earlier_broader_full_and_partial_signed_evidence_reused": True,
              "legacy_signed_receipt_without_plan_reused": True,
              "original_failure_and_first_timing_preserved": True,
              "newly_required_profile_refused": True,
              "required_normal_failure_still_fails": True,
              "actual_pr_push_queue_selectors_keep_lint_and_provider_scope": True,
              "workflow_changes_require_real_daemon_and_lint_without_paid_calls": True,
              "selector_only_changes_retain_executor_controls_without_product_jobs": True,
              "actual_cli_package_is_binary_only_and_has_no_workspace_dependents": True,
              "broken_independent_cli_does_not_build_into_cohost": True,
              "shared_runtime_source_rebuilds_actual_cohost": True,
              "workflow_scope": workflow,
              "policy_fixture_scope": policy_fixtures,
              "scheduling_scope": scheduling,
              "compiler_priority": expected_priority,
              "scope": "Actual Git/public planner and Cargo/native canaries. No product runtime, provider or performance coverage claimed."}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(f"PASS: {len(cases)} actual Git boundaries, native development case and unchanged normal binary; {output}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    scope = parser.add_mutually_exclusive_group()
    scope.add_argument("--policy-fixtures-only", action="store_true",
                       help="Run only the real Git policy-fixture controls under the existing host lease")
    scope.add_argument("--pinned-cache-command-only", action="store_true",
                       help="Run only the Linux dependency-cache command with the hosted Node prerequisite")
    args = parser.parse_args()
    if args.pinned_cache_command_only:
        args.output.mkdir(parents=True, exist_ok=False)
        pinned_cache_command(args.output.resolve(), os.environ)
    elif args.policy_fixtures_only:
        spec = importlib.util.spec_from_file_location("policy_fixture_host_lease", SOURCE / ".github/scripts/local_acceptance.py")
        helper = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(helper)
        helper.ROOT = SOURCE
        head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=SOURCE, text=True).strip()
        attempt = {"active_profile": "private-preflight-policy-fixture-controls"}
        with helper.host_acceptance_lease(attempt, head):
            policy_fixture_scope(args.output.resolve(), os.environ)
    else:
        run(args.output.resolve())
