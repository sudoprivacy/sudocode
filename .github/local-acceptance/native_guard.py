"""Run native policy enforcement from one matching protected tool set."""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import sys


SOURCE = Path(__file__).resolve().parents[2]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--executable", type=Path, required=True)
    parser.add_argument("--source-receipt", type=Path, required=True)
    parser.add_argument("--options", required=True)
    args = parser.parse_args()
    repository, output = args.repository.resolve(), args.output.resolve()
    if output == SOURCE or SOURCE in output.parents or output == repository or repository in output.parents:
        raise RuntimeError("Keep native guard outputs outside the source checkout")
    output.mkdir(parents=True, exist_ok=False)
    spec = importlib.util.spec_from_file_location("native_guard_profiles", SOURCE / ".github/local-acceptance/checks.py")
    profiles = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(profiles)
    print("GUARD_NATIVE_TOOL_PREFLIGHT: prepare and verify the platform-pinned protoc before "
          "the native matrix; isolated fixtures copy only that tool cache, never home or lease claims.", flush=True)
    protoc = profiles.cohost_protoc()
    if protoc is not None:
        (output / "protoc-source.json").write_text(json.dumps({
            "version": "3.20.2", "executable": str(protoc),
            "executable_sha256": hashlib.sha256(protoc.read_bytes()).hexdigest(),
        }, indent=2) + "\n")
    spec = importlib.util.spec_from_file_location("native_executor", SOURCE / ".github/scripts/local_acceptance.py")
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    helper.ROOT = repository
    # The isolated tool snapshot needs Git discovery for fixture-environment cleanup.
    commands = [helper.run_command(["git", "init", "--initial-branch=main", str(SOURCE)],
                                  output, output / "snapshot-git.log")]
    configured = helper.git("config", "--get", "localAcceptance.vsdevcmd", check=False).stdout.decode().strip()
    if configured:
        commands.append(helper.run_command(["git", "config", "localAcceptance.vsdevcmd", configured],
                                          SOURCE, output / "toolchain-config.log"))
    fixtures = []

    def fixture(command, cwd, log):
        """Preserve the original argv, source directory and private command log."""
        assert cwd == SOURCE
        fixtures.append({"command": list(map(str, command)), "log": str(log)})

    fixture([
        sys.executable, str(SOURCE / "e2e/local-acceptance/inherited_lease_acceptance.py"),
        "--output", str(output / "inherited-lease"),
    ], SOURCE, output / "inherited-lease.log")
    fixture([
        sys.executable, str(SOURCE / "e2e/local-acceptance/contract_reads_acceptance.py"),
        "--output", str(output / "contract-reads"),
    ], SOURCE, output / "contract-reads.log")
    fixture([
        sys.executable, str(SOURCE / "e2e/local-acceptance/parallel_acceptance.py"),
        "--output", str(output / "parallel-acceptance"),
    ], SOURCE, output / "parallel-acceptance.log")
    fixture([
        sys.executable, str(SOURCE / "e2e/local-acceptance/docker_health_acceptance.py"),
        "--output", str(output / "docker-health"),
    ], SOURCE, output / "docker-health.log")
    fixture([
        sys.executable, str(SOURCE / "e2e/local-acceptance/native.py"),
    ], SOURCE, output / "native-smoke.log")
    fixture([
        sys.executable, str(SOURCE / "e2e/local-acceptance/power_acceptance.py"),
        "--output", str(output / "power"),
    ], SOURCE, output / "power.log")
    fixture([
        sys.executable, str(SOURCE / ".github/local-acceptance/nextest_guard.py"),
        "--repository", str(repository), "--output", str(output / "nextest-executor"),
        "--executable", str(args.executable), "--source-receipt", str(args.source_receipt),
        "--options", args.options,
    ], SOURCE, output / "nextest.log")
    fixture([
        sys.executable, str(SOURCE / "e2e/local-acceptance/release_acceptance.py"),
        "--output", str(output / "release-executor"),
    ], SOURCE, output / "release.log")
    artifact_output = output / "release-artifact"
    if os.name == "nt" and len(str(artifact_output)) < 180:
        # Exercise a real nested evidence path that used to exceed MAX_PATH
        # once Cargo appended its build-script executable name.
        artifact_output /= "native-" + "x" * max(1, 180 - len(str(artifact_output)) - 8)
    fixture([
        sys.executable, str(SOURCE / "e2e/local-acceptance/release_artifact_acceptance.py"),
        "--output", str(artifact_output),
    ], SOURCE, output / "release-artifact.log")
    fixture([
        sys.executable, str(SOURCE / "e2e/local-acceptance/grouped_targets_acceptance.py"),
        "--repository", str(repository), "--output", str(output / "grouped-targets"),
        "--executable", str(args.executable), "--options", args.options,
    ], SOURCE, output / "grouped-targets.log")
    fixture([
        sys.executable, str(SOURCE / "e2e/local-acceptance/examples_acceptance.py"),
        "--output", str(output / "examples"),
    ], SOURCE, output / "examples.log")
    fixture([
        sys.executable, str(SOURCE / "e2e/local-acceptance/cohost_acceptance.py"),
        "--output", str(output / "cohost"),
    ], SOURCE, output / "cohost.log")
    fixture([
        sys.executable, str(SOURCE / "e2e/local-acceptance/native_cache_acceptance.py"),
        "--output", str(output / "native-cache"),
    ], SOURCE, output / "native-cache.log")
    fixture([
        sys.executable, str(SOURCE / "e2e/local-acceptance/native_lint_acceptance.py"),
        "--output", str(output / "native-lint"),
    ], SOURCE, output / "native-lint.log")
    protocol = [row for row in fixtures if Path(row["command"][1]).name == "parallel_acceptance.py"]
    native = [row for row in fixtures if row not in protocol]
    assert len(protocol) == 1 and len(native) == 13
    configuration = output / "native-parallel.json"
    configuration.write_text(json.dumps({
        "version": 1, "kind": "native-policy", "root": str(SOURCE), "branches": [
            {"name": "protocol", "commands": protocol},
            {"name": "native", "commands": native},
        ],
    }, indent=2) + "\n", encoding="utf-8")
    print("NATIVE_GUARD_OVERLAP: original Git/SSH/HTTP controls overlap one serial compiler/test lane; "
          "every command, assertion and log remains required. The existing shared supervisor cancels "
          "only this invocation's children on failure or interrupt.", flush=True)
    supervision = helper.run_command([
        sys.executable, str(SOURCE / ".github/local-acceptance/parallel_guard.py"),
        "--configuration", str(configuration),
    ], SOURCE, output / "native-parallel.log")
    outcome = json.loads(configuration.with_suffix(".result.json").read_text())
    assert outcome["status"] == "passed" and set(outcome["branches"]) == {"protocol", "native"}
    assert all(row["exit_code"] == 0 for row in outcome["branches"].values())
    for specification in fixtures:
        log = Path(specification["log"])
        result = json.loads(log.with_suffix(".command.json").read_text())
        expected = [shutil.which(specification["command"][0]) or specification["command"][0], *specification["command"][1:]]
        assert result["command"] == expected and result["exit_code"] == 0
        assert result["log_sha256"] == hashlib.sha256(log.read_bytes()).hexdigest()
        commands.append(result)
    commands.append(supervision)
    (output / "receipt.json").write_text(json.dumps({
        "repository": str(repository), "commands": commands,
    }, indent=2))
    print(f"Native policy guard passed: signed recovery, power admission, nextest, release, grouping, examples, "
          f"cohost, shared-cache binding and early lint enforcement; {output}")


if __name__ == "__main__":
    main()
