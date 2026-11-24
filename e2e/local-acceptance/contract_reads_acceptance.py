"""Exercise committed contract hashing through actual Git objects and refs."""
import argparse
import importlib.util
import json
from pathlib import Path
import subprocess
import time


SOURCE = Path(__file__).resolve().parents[2]


def check_projection_fingerprints(helper):
    # Exact vectors from the verifier before it shared read-only statements.
    # Each branch has a distinct projection, including the implicit fallback.
    source = b"""def shared():
    return bytes([34, 255])
def unused():
    return 17

def run(name):
    if name == 'one':
        return shared()
    elif name == 'two':
        return 2
    else:
        return 3
"""
    names = ["one", "two", "other"]
    inputs = ("executor", source, "image", None)
    for fingerprints, expected in [
        (helper.profile_commands_hashes, [
            "cf87848e8994dca360bd4ef8fd3e5e6cf87b4faa7858dd156ae3eb9a61289dff",
            "d2703263e5b828067c3a291adb7cbc32eee11da10ff3e4c658d21d9a340fc560",
            "c8fda9b41248ad591044f63879623404f246ee30f2a15be70d91b38475521747",
        ]),
        (helper.reachable_profile_commands_hashes, [
            "2b329ed08b492cb407e91dfeeed98a742c19c586783cc22aa700634bb7b2fedd",
            "07ade4a01d445f62bf635c7c5e52d49fb0721fbaf667e8089f2736d6c3572593",
            "e0900c62792bbda28eab8e42d98ad5b06226b19605aaccfa8f152e046eed46b3",
        ]),
    ]:
        assert fingerprints(None, names, inputs=inputs) == dict(zip(names, expected, strict=True))
        assert fingerprints(None, reversed(names), inputs=inputs) == dict(zip(names, expected, strict=True))


def run(output):
    output.mkdir(parents=True, exist_ok=False)
    spec = importlib.util.spec_from_file_location("contract_reads", SOURCE / ".github/scripts/local_acceptance.py")
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    root = output / "real Git 仓库 with spaces"
    root.mkdir()
    helper.ROOT = root
    commands = []
    reads = []
    read = helper.git

    def observed(*args, **kwargs):
        before = time.monotonic()
        result = read(*args, **kwargs)
        reads.append({"command": ["git", *args], "seconds": time.monotonic() - before,
                      "exit_code": result.returncode, "stdout_sha256": helper.sha(result.stdout)})
        return result

    helper.git = observed

    def git(*args, data=None):
        process = subprocess.run(["git", *args], cwd=root, input=data, capture_output=True)
        commands.append({"command": ["git", *args], "exit_code": process.returncode,
                         "stdout_hex": process.stdout.hex(), "stderr_hex": process.stderr.hex()})
        assert process.returncode == 0, process.stderr.decode(errors="replace")
        return process.stdout

    def commit(message):
        git("add", ".")
        git("-c", "commit.gpgsign=false", "commit", "-m", message)
        return git("rev-parse", "HEAD").decode().strip()

    def original(ref):
        paths = [helper.POLICY, helper.SIGNERS, ".github/scripts/local_acceptance.py",
                 ".github/workflows/local-acceptance.yml", ".github/workflows/local-acceptance-publish.yml"]
        paths += helper.value("ls-tree", "-r", "--name-only", ref, ".github/local-acceptance").splitlines()
        return helper.sha(helper.canonical({path: helper.sha(helper.tracked(ref, path)) for path in sorted(paths)}))

    git("init", "--initial-branch=main")
    git("config", "user.name", "Acceptance fixture")
    git("config", "user.email", "fixture@example.invalid")
    git("config", "core.autocrlf", "false")
    git("config", "core.quotePath", "false")
    inputs = {helper.POLICY: b'{"version":1}\n', helper.SIGNERS: b'{"signers":{}}\n',
              ".github/scripts/local_acceptance.py": b"# committed runner\n",
              ".github/workflows/local-acceptance.yml": b"name: acceptance\n",
              ".github/workflows/local-acceptance-publish.yml": b"name: publish\n",
              ".github/local-acceptance/binary.dat": bytes(range(256)) * 8192 + b"\n\0tail",
              ".github/local-acceptance/empty.py": b"",
              ".github/local-acceptance/duplicate.py": b"# committed runner\n",
              ".github/local-acceptance/中文 space.py": b"print('never execute candidate code')\n",
              ".gitattributes": b".github/local-acceptance/binary.dat export-ignore\n"}
    for name, data in inputs.items():
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
    first = commit("Original binary and Unicode contract")
    observations = []
    for label, ref in [("original", first), ("mutable-before", "HEAD")]:
        before = time.monotonic()
        expected = original(ref)
        original_seconds = time.monotonic() - before
        original_processes = len(reads)
        reads.clear()
        before = time.monotonic()
        actual = helper.contract_hash(ref)
        assert actual == expected
        assert len(reads) == 2, "Contract reads must not launch one Git process per file"
        observations.append({"label": label, "sha256": actual,
                             "original_seconds": original_seconds, "batch_seconds": time.monotonic() - before,
                             "original_git_processes": original_processes, "batch_git_processes": len(reads)})
        reads.clear()
    # Working-tree edits must not influence committed bytes. Archive attributes
    # must not omit binary data, and a moved ref must not reuse an old result.
    changed = root / ".github/local-acceptance/binary.dat"
    changed.write_bytes(b"changed\0\n")
    assert helper.contract_hash("HEAD") == observations[0]["sha256"]
    second = commit("Change the export-ignored committed blob")
    assert helper.contract_hash("HEAD") == original(second) != observations[0]["sha256"]
    assert helper.contract_hash(first) == observations[0]["sha256"]
    # An indexed symbolic link is read as its committed link text, never by
    # following a filesystem target. This works on Windows without symlink UAC.
    link = git("hash-object", "-w", "--stdin", data=b"../outside-secret").decode().strip()
    git("update-index", "--add", "--cacheinfo", "120000," + link + ",.github/local-acceptance/link.py")
    git("-c", "commit.gpgsign=false", "commit", "-m", "Committed link text")
    assert helper.contract_hash("HEAD") == original("HEAD")
    git("update-index", "--add", "--cacheinfo", "160000," + first + ",.github/local-acceptance/nested")
    git("-c", "commit.gpgsign=false", "commit", "-m", "Unsupported nested Git entry")
    try:
        helper.contract_hash("HEAD")
    except RuntimeError as error:
        assert "non-blob" in str(error)
    else:
        raise AssertionError("Contract accepted a nested Git repository")
    git("rm", "--cached", ".github/local-acceptance/nested")
    git("rm", "--cached", helper.SIGNERS)
    git("-c", "commit.gpgsign=false", "commit", "-m", "Missing required input")
    for label, operation in [("missing-required", lambda: helper.contract_hash("HEAD")),
                             ("unknown-ref", lambda: helper.contract_hash("missing-ref")),
                             ("wrong-object-type", lambda: helper.committed_blob_bytes({"commit": first})),
                             ("missing-object", lambda: helper.committed_blob_bytes({"lost": "0" * 40}))]:
        try:
            operation()
        except (RuntimeError, ValueError):
            pass
        else:
            raise AssertionError(f"Committed read accepted {label}")
    assert helper.committed_blob_bytes({}) == {}

    executor = ".github/scripts/local_acceptance.py"
    checks = ".github/local-acceptance/checks.py"
    image = ".github/local-acceptance/Dockerfile"
    command = b"def run_command():\n    return 1"
    (root / executor).write_bytes(command + b"\n")
    (root / checks).write_bytes(b"def run(name):\n    if name == 'one':\n        return 1\n    else:\n        return 2\n")
    (root / image).write_bytes(b"FROM scratch\n")
    policy = commit("Execution fingerprint inputs")
    names = ["one", "two"]
    original_command_hash = helper.sha(helper.canonical({"executor": helper.sha(command),
        checks: helper.sha(helper.tracked(policy, checks)), image: helper.sha(helper.tracked(policy, image))}))
    assert helper.commands_hash(policy) == original_command_hash
    receipt = {"head": policy, "commands_ref": policy, "commands_sha256": original_command_hash,
               "profile_commands_sha256": helper.profile_commands_hashes(policy, names),
               "reachable_profile_commands_sha256": helper.reachable_profile_commands_hashes(policy, names)}
    for ref, expected_calls in [(policy, 2), ("HEAD", 3)]:
        reads.clear()
        assert helper.compatible_profile_commands(receipt, ref, names)
        assert len(reads) == expected_calls, reads
    (root / image).write_bytes(b"FROM scratch\n# new image recipe\n")
    commit("Move the policy ref to different execution inputs")
    assert not helper.compatible_profile_commands(receipt, "HEAD", names)
    assert helper.compatible_profile_commands(receipt, policy, names)
    (root / image).unlink()
    commit("Remove a required execution input")
    assert not helper.compatible_profile_commands(receipt, "HEAD", names)
    check_projection_fingerprints(helper)

    result = {"status": "PASS", "observations": observations, "commands": commands, "committed_reads": reads,
              "exact_binary_empty_duplicate_unicode_and_link_bytes": True,
              "working_tree_ignored_and_moved_ref_refreshed": True,
              "archive_attributes_do_not_omit_contract": True,
              "missing_required_ref_and_object_refused": True,
              "execution_fingerprint_snapshot_reused_only_within_one_verification": True,
              "both_original_profile_hash_versions_and_order_independence_verified": True,
              "scope": "Real Git commits/object storage and original hash reference. No signature/test/coverage requirements removed."}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({key: value for key, value in result.items() if key not in {"commands", "committed_reads"}}), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    run(args.output.resolve())
