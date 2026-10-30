"""Run the protected native guard against an unchanged, incompatible Git checkout.

This verifies tool ownership and enforcement with real Git/Cargo/child processes;
it does not claim product PTY or live-provider coverage.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import time


SOURCE = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    output = (args.output or Path.home() / ".nexus/local-acceptance/policy-guard" / str(time.time_ns())).resolve()
    if output == SOURCE or SOURCE in output.parents:
        raise RuntimeError("Keep guard fixtures outside the source checkout")
    output.mkdir(parents=True, exist_ok=False)
    parent = load("policy_guard_parent", SOURCE / "e2e/local-acceptance/run.py")
    os.environ.clear()
    os.environ.update(parent.FIXTURE_ENV)
    authority = load("policy_guard_authority", SOURCE / ".github/scripts/local_acceptance.py")
    profiles = load("policy_guard_profiles", SOURCE / ".github/local-acceptance/checks.py")
    # Capture the actual policy tool set before pointing Git helpers at fixtures.
    policy = json.loads((SOURCE / "spec/local_acceptance.json").read_text())
    recipe = policy["checks"]["guard-contract"]["executor"]
    paths = set(authority.contract_files("HEAD")) | {recipe["entry_point"], *recipe["support_files"]}
    contents = {path: (SOURCE / path).read_bytes() for path in paths}
    toolchain = subprocess.run(["git", "config", "--get", "localAcceptance.vsdevcmd"],
                              cwd=SOURCE, text=True, capture_output=True).stdout.strip()
    candidate = output / "old-candidate"
    candidate.mkdir()
    authority.ROOT = candidate

    def git(*arguments):
        return subprocess.check_output(["git", *arguments], cwd=candidate, text=True).strip()

    def commit(subject):
        git("add", ".")
        git("commit", "-m", subject)
        return git("rev-parse", "HEAD")

    git("init", "--initial-branch=main")
    git("config", "user.name", "Policy guard fixture")
    git("config", "user.email", "policy-guard@example.invalid")
    git("config", "localAcceptance.signer", "no-fixture-signing")
    git("config", "localAcceptance.key", str(output / "no-private-key"))
    if toolchain:
        git("config", "localAcceptance.vsdevcmd", toolchain)
    old_files = {
        "README.md": "An old candidate must not acquire new guard files.\n",
        ".github/local-acceptance/checks.py": 'raise RuntimeError("candidate profile helper was imported")\n',
        ".github/scripts/local_acceptance.py": 'raise RuntimeError("candidate executor was imported")\n',
        "e2e/local-acceptance/run.py": 'import os\nfrom pathlib import Path\n'
            'assert not Path("e2e/local-acceptance/nextest.py").exists()\n'
            'Path(os.environ["OLD_GUARD_MARKER"]).write_text("candidate-owned guard ran")\n',
    }
    for path, text in old_files.items():
        destination = candidate / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(text)
    old = commit("Old candidate without native guard helpers")
    for path, data in contents.items():
        destination = candidate / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(data)
    protected = commit("Immutable protected executor policy")
    git("update-ref", "refs/remotes/origin/main", protected)
    git("switch", "--detach", old)
    # The former mandatory command really fails; never count this as product coverage.
    print("Expected negative: the old checkout lacks the newly added guard path.", flush=True)
    try:
        authority.run_command([sys.executable, "e2e/local-acceptance/nextest.py"], candidate,
                              output / "old-path-negative.log")
        raise AssertionError("The old path unexpectedly exists")
    except RuntimeError:
        negative = json.loads((output / "old-path-negative.command.json").read_text())
        assert negative["exit_code"] == 2, negative
    marker = output / "candidate-guard.txt"

    def execute(command, cwd, log, environment=None):
        return authority.run_command(command, cwd, log, {**(environment or {}), "OLD_GUARD_MARKER": str(marker)})

    profiles.run.execution_refs = (protected, old)
    result = profiles.run("guard-contract", candidate, output / "positive", old, execute)
    assert len(result) == 2 and all(row["exit_code"] == 0 for row in result)
    assert marker.read_text() == "candidate-owned guard ran"
    assert git("rev-parse", "HEAD") == old and not git("status", "--porcelain")
    assert not (candidate / "e2e/local-acceptance/nextest.py").exists()
    native = json.loads((output / "positive/nextest-executor/receipt.json").read_text())
    assert native["repository"] == str(candidate)
    assert [row["command"]["exit_code"] for row in native["checks"]] == [0, 100, 4]
    bound = authority.profile_executor_inputs(protected, "guard-contract")
    for path, digest in bound.items():
        actual = output / "positive/native-guard-policy" / path
        assert hashlib.sha256(actual.read_bytes()).hexdigest() == digest, path
    names = list(policy["checks"])
    fingerprint_results = []
    for relative in [recipe["entry_point"], *recipe["support_files"]]:
        git("switch", "--detach", protected)
        path = candidate / relative
        path.write_bytes(path.read_bytes() + b"\n# Changed protected dependency.\n")
        changed = commit("Change a protected guard dependency")
        for method in [authority.profile_commands_hashes, authority.reachable_profile_commands_hashes]:
            before, after = method(protected, names), method(changed, names)
            affected = {name for name in names if before[name] != after[name]}
            assert affected == {"guard-contract"}, (relative, method.__name__, affected)
            fingerprint_results.append({"file": relative, "encoding": method.__name__, "affected": sorted(affected)})
    git("switch", "--detach", protected)
    invalid = json.loads((candidate / "spec/local_acceptance.json").read_text())
    invalid["checks"]["guard-contract"]["executor"]["support_files"].append("../escape.py")
    (candidate / "spec/local_acceptance.json").write_text(json.dumps(invalid))
    bad = commit("Reject an executor outside the protected contract")
    rejected_output = output / "must-not-start"
    try:
        authority.check(argparse.Namespace(head=bad, base=protected, policy_ref=bad,
                        output=str(rejected_output), publish=False, remote="origin", resume_from=None))
        raise AssertionError("Unprotected executor input was accepted")
    except RuntimeError as error:
        assert "unique protected policy files" in str(error), error
        assert not rejected_output.exists()
    git("switch", "--detach", old)
    assert not git("status", "--porcelain")
    (output / "receipt.json").write_text(json.dumps({
        "candidate": old, "protected": protected, "commands": result,
        "actual_legacy_missing_path_refused": negative,
        "actual_native_child_and_failure_and_empty_selection": native,
        "only_owner_fingerprint_changes": fingerprint_results,
        "unprotected_input_refused_before_commands": True,
        "old_candidate_head_and_checkout_unchanged": True,
    }, indent=2))
    print(f"Policy-owned guard E2E passed: unchanged old checkout, real native child/refusals, "
          f"both fingerprint encodings and unprotected-input refusal; {output}")


if __name__ == "__main__":
    main()
