"""Verify frozen Git inputs and image identity through the real Docker launcher.

The image calls the production main/dispatcher with a small native workload in
place of statistical benchmarks. No shared benchmark checkout, target, lease or
developer credentials are mounted by this fixture.
"""

import argparse
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import uuid


SOURCE = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def run(output, base_image):
    output.mkdir(parents=True, exist_ok=False)
    root = output / "repository"
    root.mkdir()
    checks = load("container_profiles", SOURCE / ".github/local-acceptance/checks.py")
    executor = load("container_executor", SOURCE / ".github/scripts/local_acceptance.py")
    token = uuid.uuid4().hex
    lease = f"sudocode-container-inputs-{token}"
    cache = lease + "-cache-v1"
    storage = output / "private-cache"
    checks.container_storage = lambda: (storage, lease)
    commands, tags, images = [], set(), set()

    def docker_operation(row, action):
        return Path(row["command"][0]).stem.lower() == "docker" and row["command"][1:2] == [action]

    def execute(arguments, cwd=root, extra=None, log=None):
        log = log or output / "commands" / f"{len(commands):03d}.log"
        commands.append({"command": list(map(str, arguments)), "log": str(log)})
        try:
            result = executor.run_command(arguments, cwd, log, extra)
        except RuntimeError:
            receipt = log.with_suffix(".command.json")
            if receipt.exists():
                commands[-1].update(json.loads(receipt.read_text()))
            raise
        commands[-1].update(result)
        return log

    def git(*arguments):
        return execute(["git", *arguments]).read_text(encoding="utf-8").strip()

    git("init", "--initial-branch=main")
    git("config", "user.name", "Container input fixture")
    git("config", "user.email", "container-inputs@example.invalid")
    probe = '''import hashlib,importlib.util,json,shutil,subprocess,sys
from pathlib import Path
expected=json.loads(Path('/evidence/expected.json').read_text())
image=Path('/fixture-policy').read_text()
assert image==expected['policy'],f'policy image mismatch: {image} != {expected}'
actual={label:(Path('/inputs')/label/'marker.txt').read_text()
        for label in ['candidate','baseline','trusted']}
assert actual==expected['sources'],f'Git snapshot mismatch: {actual} != {expected}'
assert not any((Path('/inputs')/label/'private-untracked.txt').exists() for label in actual)
package=json.loads(Path('/context/package.json').read_text())
assert package['fixturePolicy']==expected['policy'],'image context used another policy'
spec=importlib.util.spec_from_file_location('actual_profiles','/inputs/runner/.github/local-acceptance/checks.py')
checks=importlib.util.module_from_spec(spec);spec.loader.exec_module(checks)
observed={'sources':actual,'image_policy':image}
def workload(name,root,output,base,command,target):
    # Keep the real main(), run(), output creation and command executor. Only
    # replace the expensive product workload; it must use the supplied output.
    reused=checks.verified_candidate_release(root.parent) if name=='browser' else None
    assert bool(reused)==expected.get('reuse',False),'unexpected release reuse decision'
    binary=output/'scode-candidate'
    shutil.copy2(reused or sys.executable,binary)
    assert binary.read_bytes()==Path(sys.executable).read_bytes(),'another candidate release was reused'
    log=command(str(binary),'-c','print("REUSED_NATIVE_BINARY_EXECUTED")')
    assert log.read_text().strip()=='REUSED_NATIVE_BINARY_EXECUTED','staged native Linux executable cannot run'
    report=output/'measurements/report.json'
    report.parent.mkdir(parents=True)
    report.write_text(json.dumps({'status':'passed','candidate_sha256':hashlib.sha256(binary.read_bytes()).hexdigest()}))
    observed.update(reused=bool(reused),profile_output=str(output.relative_to('/evidence')))
checks.benchmark=workload
assert sys.argv[1]=='/inputs/runner/.github/local-acceptance/checks.py'
sys.argv=sys.argv[1:]
checks.main()
profile=Path('/evidence')/observed['profile_output']
rows=json.loads((profile/'commands.json').read_text())
assert len(rows)==1 and rows[0]['exit_code']==0,'production main did not persist the native command'
Path('/evidence/observed.json').write_text(json.dumps(observed))
print('FROZEN_CONTAINER_INPUTS_PASS',flush=True)
'''

    def commit(label, baseline):
        files = {
            "marker.txt": label,
            "probe.py": probe,
            ".github/local-acceptance/Dockerfile": f'FROM {base_image}\n'
                'RUN pip install --no-cache-dir psutil==7.2.2\n'
                f'LABEL scode.container-input-fixture="{token}"\n'
                f'RUN python3 -c "from pathlib import Path; Path(\'/fixture-policy\').write_text(\'{label}\')"\n'
                'COPY e2e/terminal-resize/package*.json /context/\n'
                'ENTRYPOINT ["python3","/inputs/candidate/probe.py"]\n',
            "e2e/terminal-resize/package.json": json.dumps({"name": "container-input-fixture", "fixturePolicy": label}),
            "e2e/terminal-resize/package-lock.json": '{"lockfileVersion":3}',
            "e2e/terminal-resize/performance-policy.json": json.dumps({"baseline_commit": baseline}),
        }
        for relative, content in files.items():
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content, encoding="utf-8")
        git("add", ".")
        git("commit", "-m", f"Fixture {label}")
        return git("rev-parse", "HEAD")

    baseline = commit("baseline", "not-selected")
    protected = commit("main-policy", baseline)
    git("update-ref", "refs/remotes/origin/main", protected)
    candidate = commit("candidate-policy", protected)
    (root / "private-untracked.txt").write_text("An untracked fixture must never enter the container.")
    configuration = {"baseline": baseline, "protected": protected, "candidate": candidate,
                     "base_image": base_image, "fixture_label": token, "cache_volume": cache}
    (output / "configuration.json").write_text(json.dumps(configuration, indent=2))
    execute(["docker", "volume", "create", "--label", f"scode.container-input-fixture={token}", lease])
    execute(["docker", "volume", "create", "--label", f"scode.container-input-fixture={token}", cache])

    def launch(name, policy_ref, expected, *, swap_image=None, profile="rendering", corrupt=None, parent=output):
        evidence = parent / name
        evidence.mkdir(parents=True)
        (evidence / "expected.json").write_text(json.dumps(expected))

        def execute_profile(arguments, cwd, log, environment=None):
            if list(arguments[:2]) == ["docker", "run"]:
                # The launcher normally stages its executor from the callback's
                # source file. This fixture wraps that callback to observe real
                # Docker operations, so stage the actual executor it delegates to.
                shutil.copyfile(SOURCE / ".github/scripts/local_acceptance.py",
                                evidence / "inputs/runner/.github/scripts/local_acceptance.py")
            if list(arguments[:2]) == ["docker", "run"] and corrupt:
                reused = evidence / "inputs/candidate-release"
                if corrupt == "bytes":
                    (reused / "scode").write_bytes(b"different candidate bytes")
                else:
                    receipt = reused / "identity.json"
                    value = json.loads(receipt.read_text())
                    value[corrupt] = "wrong-" + value[corrupt]
                    receipt.write_text(json.dumps(value))
            execute(arguments, cwd=cwd, extra=environment, log=log)
            if list(arguments[:2]) == ["docker", "build"]:
                tag = arguments[arguments.index("--tag") + 1]
                tags.add(tag)
                image = (evidence / "image-id.txt").read_text().strip()
                images.add(image)
                if swap_image:
                    # Actual concurrent changes after the build: the launched
                    # image and snapshots must still match the captured inputs.
                    execute(["docker", "tag", swap_image, tag])
                    git("update-ref", "refs/remotes/origin/main", baseline)
                    git("update-ref", "refs/heads/main", baseline)
            return json.loads(log.with_suffix(".command.json").read_text())

        checks.run.execution_refs = (policy_ref, candidate)
        checks.run(profile, root, evidence, protected, execute_profile)
        provenance = json.loads((evidence / "container-provenance.json").read_text())
        observed = json.loads((evidence / "observed.json").read_text())
        assert observed["sources"] == expected["sources"]
        assert observed["reused"] == expected.get("reuse", False)
        assert observed["profile_output"] == f"container/{profile}"
        assert not (evidence / "scode-candidate").exists(), "Fixture bypassed the production output layout"
        assert provenance["candidate"] == candidate and provenance["policy"] == policy_ref
        assert provenance["baseline"] == (protected if profile == "browser" else
                                           baseline if policy_ref == protected else protected)
        actual_run = next(row["command"] for row in commands[::-1]
                          if docker_operation(row, "run"))
        assert provenance["image_id"] in actual_run and provenance["image_tag"] not in actual_run
        assert f"type=volume,source={cache},target=/state" in actual_run
        assert f"type=bind,source={storage},target=/legacy-state,readonly" in actual_run
        return provenance

    try:
        first = launch("protected-policy", protected, {"policy": "main-policy", "sources": {
            "candidate": "candidate-policy", "baseline": "baseline", "trusted": "main-policy"}})
        # Another author can build the exact same context while the rendering
        # container is running. BuildKit exports a fresh attestation/index even
        # on a cache hit; sharing its tag can discard the first index before the
        # original invocation starts its browser container.
        browser_expected = {"policy": "main-policy", "reuse": False, "sources": {
            "candidate": "candidate-policy", "baseline": "main-policy", "trusted": "main-policy"}}
        independent = launch("same-context-independent-builder", protected, browser_expected,
                             profile="browser", parent=output / "independent-invocation")
        retained = launch("browser-after-independent-build", protected,
                          {**browser_expected, "reuse": True}, profile="browser")
        assert retained["image_id"] == first["image_id"]
        assert retained["image_tag"] == first["image_tag"], "Reuse lost the original image's owned reference"
        assert independent["image_tag"] != first["image_tag"], "Independent builds share a mutable image reference"
        print("PASS: another real same-context build preserves the original image and executable for browser reuse", flush=True)
        second = launch("candidate-policy-ref-and-tag-change", candidate, {"policy": "candidate-policy", "sources": {
            "candidate": "candidate-policy", "baseline": "main-policy", "trusted": "candidate-policy"}},
            swap_image=first["image_id"])
        assert git("rev-parse", "HEAD") == baseline, "Fixture did not actually move HEAD"
        assert git("rev-parse", "origin/main") == baseline, "Fixture did not actually move origin/main"
        tag_id = execute(["docker", "image", "inspect", "--format", "{{.Id}}", second["image_tag"]]).read_text().strip()
        assert tag_id == first["image_id"] != second["image_id"], "Fixture did not actually replace the tag"
        print("PASS: actual Git ref moves and Docker retag preserve frozen input/image selection", flush=True)
        expected = {"policy": "main-policy", "reuse": True, "sources": {
            "candidate": "candidate-policy", "baseline": "main-policy", "trusted": "main-policy"}}
        # Replace the original rendering tag too. Same-invocation browser reuse
        # must launch the retained immutable image ID, never the mutable tag.
        execute(["docker", "tag", second["image_id"], first["image_tag"]])
        before = sum(docker_operation(row, "build") for row in commands)
        reused = launch("same-invocation-browser", protected, expected, profile="browser")
        assert reused["image_id"] == first["image_id"]
        assert sum(docker_operation(row, "build") for row in commands) == before
        for corrupt in ["bytes", "candidate", "policy", "image_id", "context_sha256"]:
            try:
                launch("negative-reuse-" + corrupt, protected, expected, profile="browser", corrupt=corrupt)
            except RuntimeError:
                errors = [Path(row["log"]).read_text(errors="replace") for row in commands
                          if docker_operation(row, "run")]
                assert ("release bytes changed" in errors[-1] if corrupt == "bytes"
                        else "different source, policy or image" in errors[-1]), errors[-1]
            else:
                raise AssertionError(f"Tampered reuse {corrupt} was accepted")
        # A different invocation's output root cannot inherit this memory entry,
        # even for the exact same Git inputs. Browser-only execution stays valid.
        launch("fresh-browser", protected, {**expected, "reuse": False}, profile="browser",
               parent=output / "another-invocation")
        assert sum(docker_operation(row, "build") for row in commands) == before + 1
        print("PASS: exact invocation reuses a frozen image/release; changed identity/bytes rejected; fresh invocation builds normally", flush=True)
        print("Expected negative case: the following wrong-policy container must be rejected", flush=True)
        try:
            launch("negative-wrong-policy", protected, {"policy": "candidate-policy", "sources": {
                "candidate": "candidate-policy", "baseline": "main-policy", "trusted": "candidate-policy"}})
        except RuntimeError:
            errors = [Path(row["log"]).read_text(errors="replace") for row in commands
                      if docker_operation(row, "run")]
            assert "policy image mismatch" in errors[-1], errors[-1]
            print("PASS: wrong-policy container rejected by the real input probe", flush=True)
        else:
            raise AssertionError("A real container using the wrong policy was accepted")
        result = {"status": "PASS", "cases": 12,
            "production_main_and_dispatcher_layout": True,
            "same_invocation_image_and_binary_reused": True, "fresh_invocation_does_not_reuse": True,
            "independent_same_context_build_preserves_original_image": True,
            "changed_identity_and_bytes_refused": True,
            "scope": "Real production container launcher, main/dispatcher, command executor, Git commits/archives and Docker builds/runs. The native workload replaces statistical benchmarks and writes only to the output supplied by the dispatcher. Protected/candidate policies and actual ref/tag moves retain frozen inputs. Same-invocation browser reuses the exact image and executes a retained native Linux binary after host staging; another invocation builds normally. Five corrupted release identities/bytes and a wrong-policy image are rejected inside real containers. Probe executable is the image's Python binary; actual Cargo/source/feature identity is covered by benchmark_targets_acceptance.py. No product benchmark coverage."}
    finally:
        (output / "commands.json").write_text(json.dumps(commands, indent=2))
        # All tags and IDs below were produced by this UUID-labelled fixture.
        for target in [*sorted(tags), *sorted(images)]:
            inspected = subprocess.run(["docker", "image", "inspect", "--format",
                '{{index .Config.Labels "scode.container-input-fixture"}}', target],
                capture_output=True, text=True)
            if inspected.returncode:
                continue
            if inspected.stdout.strip() != token:
                raise RuntimeError(f"Refusing to clean an image outside this fixture: {target}")
            subprocess.run(["docker", "image", "rm", target], check=True, capture_output=True)
        for volume in [lease, cache]:
            inspected = subprocess.check_output(["docker", "volume", "inspect", "--format",
                '{{index .Labels "scode.container-input-fixture"}}', volume], text=True).strip()
            if inspected != token:
                raise RuntimeError(f"Refusing to clean a volume outside this fixture: {volume}")
            subprocess.run(["docker", "volume", "rm", volume], check=True, capture_output=True)
    (output / "result.json").write_text(json.dumps(result, indent=2))
    print(json.dumps(result), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--base-image", default="python:3.12-slim")
    options = parser.parse_args()
    run(options.output.resolve(), options.base_image)
