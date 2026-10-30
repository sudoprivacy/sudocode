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
    previous = sys.dont_write_bytecode
    sys.dont_write_bytecode = True
    try:
        spec.loader.exec_module(module)
    finally:
        sys.dont_write_bytecode = previous
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
    pins = profiles.native_guard_files()
    # The Docker decision matrix starts from a tracked-source archive without Git history.
    # Copy the policy directories, then let the real Git contract select its own inputs.
    contents = {
        path.relative_to(SOURCE).as_posix(): path.read_bytes()
        for directory in [SOURCE / ".github", SOURCE / "spec", SOURCE / "e2e/local-acceptance", SOURCE / "e2e/startup"]
        for path in directory.rglob("*")
        if path.is_file() and "__pycache__" not in path.parts
    }
    # Windows git archive can apply core.autocrlf to the Docker input snapshot.
    # Reconstruct only exact pinned policy bytes for this private Git fixture;
    # arbitrary changed content must still fail the production SHA256 check.
    for path, expected in pins.items():
        canonical = contents[path].replace(b"\r\n", b"\n")
        if hashlib.sha256(canonical).hexdigest() != expected:
            raise RuntimeError(f"Guard fixture source differs from its protected byte pin: {path}")
        contents[path] = canonical
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
    git("config", "core.autocrlf", "false")
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
    entry = profiles.stage_profile_executor(candidate, protected, "guard-contract", output / "native-guard-policy")
    executable = profiles.nextest_binary(output)
    result = authority.run_command([
        sys.executable, str(entry), "--repository", str(candidate), "--output", str(output / "positive"),
        "--executable", str(executable), "--source-receipt", str(output / "nextest-source.json"),
        "--options", json.dumps(profiles.nextest_options(output)),
    ], candidate, output / "native-guard.log")
    assert result["exit_code"] == 0
    assert git("rev-parse", "HEAD") == old and not git("status", "--porcelain")
    assert not (candidate / "e2e/local-acceptance/nextest.py").exists()
    smoke = json.loads((output / "positive/receipt.json").read_text())
    assert smoke["repository"] == str(candidate)
    assert all(row["exit_code"] == 0 for row in smoke["commands"])
    native = json.loads((output / "positive/nextest-executor/receipt.json").read_text())
    assert native["repository"] == str(candidate)
    assert [row["command"]["exit_code"] for row in native["checks"]] == [0, 100, 4]
    release = json.loads((output / "positive/release-executor/result.json").read_text())
    assert release["status"] == "PASS"
    wrong_artifact = json.loads((output / "positive/release-executor/wrong-artifact.command.json").read_text())
    assert wrong_artifact["exit_code"] == 101
    for path, digest in pins.items():
        actual = output / "native-guard-policy" / path
        assert hashlib.sha256(actual.read_bytes()).hexdigest() == digest, path
    names = list(policy["checks"])
    fingerprint_results = []
    before = authority.reachable_profile_commands_hashes(protected, names)
    for index, (relative, expected) in enumerate(pins.items()):
        git("switch", "--detach", protected)
        path = candidate / relative
        path.write_bytes(path.read_bytes() + b"\n# Changed protected dependency.\n")
        changed = commit("Change a protected guard dependency")
        try:
            profiles.stage_profile_executor(candidate, changed, "guard-contract", output / f"rejected-input-{index}")
            raise AssertionError(f"Changed executor bytes were accepted: {relative}")
        except RuntimeError as error:
            assert "Update its protected pin before running acceptance" in str(error), error
        selector = candidate / ".github/local-acceptance/checks.py"
        actual = hashlib.sha256(path.read_bytes()).hexdigest()
        selector.write_text(selector.read_text().replace(expected, actual))
        changed = commit("Bind reviewed native guard bytes in the protected profile")
        after = authority.reachable_profile_commands_hashes(changed, names)
        affected = {name for name in names if before[name] != after[name]}
        assert affected == {"guard-contract"}, (relative, affected)
        fingerprint_results.append({"file": relative, "affected": sorted(affected), "unpinned_bytes_refused": True})
        try:
            authority.check_contract_changes(changed, protected, protected)
            raise AssertionError(f"Declared executor input escaped contract authorization: {relative}")
        except RuntimeError as error:
            assert "Candidate changes the protected acceptance contract" in str(error), error
    git("switch", "--detach", protected)
    selector = candidate / ".github/local-acceptance/checks.py"
    selector.write_text(selector.read_text().replace(next(iter(pins)), "../escape.py"))
    bad = commit("Reject an executor outside the protected contract")
    bad_profiles = load("invalid_policy_guard_paths", selector)
    try:
        bad_profiles.stage_profile_executor(candidate, bad, "guard-contract", output / "must-not-start")
        raise AssertionError("Unprotected executor input was accepted")
    except RuntimeError as error:
        assert "Executor input escaped its policy snapshot" in str(error), error
        assert not (output / "escape.py").exists()
    git("switch", "--detach", old)
    assert not git("status", "--porcelain")
    (output / "receipt.json").write_text(json.dumps({
        "candidate": old, "protected": protected, "commands": result,
        "actual_legacy_missing_path_refused": negative,
        "actual_signed_resume_smoke": smoke,
        "actual_native_child_and_failure_and_empty_selection": native,
        "actual_release_driver_and_wrong_artifact_refusal": release,
        "only_owner_fingerprint_changes": fingerprint_results,
        "unprotected_input_refused_before_commands": True,
        "old_candidate_head_and_checkout_unchanged": True,
    }, indent=2))
    print(f"Policy-owned guard E2E passed: unchanged old checkout, real native child/refusals, "
          f"guard-only fingerprints, changed-input and escaping-path refusals; {output}")


if __name__ == "__main__":
    main()
