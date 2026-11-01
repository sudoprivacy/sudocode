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
        for label in ['candidate','baseline','trusted'] if (Path('/inputs')/label).exists()}
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
    binary=output/'scode-candidate'
    reused=checks.verified_candidate_release(Path('/inputs')) if name=='browser' else None
    shutil.copy2(reused or sys.executable,binary)
    # This private probe is later deliberately corrupted by the non-root host.
    # Docker owns the output on Linux; retain execution while allowing that edit.
    binary.chmod(binary.stat().st_mode | 0o002)
    log=command(str(binary),'-c','print("STAGED_NATIVE_BINARY_EXECUTED")')
    assert log.read_text().strip()=='STAGED_NATIVE_BINARY_EXECUTED','staged native Linux executable cannot run'
    report=output/'measurements/report.json'
    report.parent.mkdir(parents=True)
    report.write_text(json.dumps({'status':'passed','candidate_sha256':hashlib.sha256(binary.read_bytes()).hexdigest()}))
    observed.update(profile_output=str(output.relative_to('/evidence')),reused_release=reused is not None)
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
        git("add", "--", *files)
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

    def launch(name, policy_ref, expected, *, swap_image=None, profile="rendering", parent=output):
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
        assert observed["profile_output"] == f"container/{profile}"
        assert not (evidence / "scode-candidate").exists(), "Fixture bypassed the production output layout"
        assert provenance["candidate"] == candidate and provenance["policy"] == policy_ref
        assert provenance.get("baseline") == (None if profile == "browser" else
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
        # on a cache hit; a shared tag could then name a different image.
        independent = launch("same-context-independent-builder", protected,
                             {"policy": "main-policy", "sources": {"candidate": "candidate-policy"}},
                             profile="browser", parent=output / "independent-invocation")
        assert independent["image_tag"] != first["image_tag"], "Independent builds share a mutable image reference"
        owned = execute(["docker", "image", "inspect", "--format", "{{.Id}}", first["image_tag"]]).read_text().strip()
        assert owned == first["image_id"], "Another same-context build replaced the original image reference"
        print("PASS: another real same-context build keeps its own tag and preserves the original image", flush=True)
        second = launch("candidate-policy-ref-and-tag-change", candidate, {"policy": "candidate-policy", "sources": {
            "candidate": "candidate-policy", "baseline": "main-policy", "trusted": "candidate-policy"}},
            swap_image=first["image_id"])
        assert git("rev-parse", "HEAD") == baseline, "Fixture did not actually move HEAD"
        assert git("rev-parse", "origin/main") == baseline, "Fixture did not actually move origin/main"
        tag_id = execute(["docker", "image", "inspect", "--format", "{{.Id}}", second["image_tag"]]).read_text().strip()
        assert tag_id == first["image_id"] != second["image_id"], "Fixture did not actually replace the tag"
        print("PASS: actual Git ref moves and Docker retag preserve frozen input/image selection", flush=True)
        # The browser smoke stages only the candidate: no baseline or trusted
        # source enters its container, and every invocation builds its image.
        before = sum(docker_operation(row, "build") for row in commands)
        launch("browser-candidate-only", protected,
               {"policy": "main-policy", "sources": {"candidate": "candidate-policy"}}, profile="browser",
               parent=output / "standalone-browser-invocation")
        assert sum(docker_operation(row, "build") for row in commands) == before + 1
        print("PASS: the browser smoke container receives only the frozen candidate snapshot", flush=True)
        before = sum(docker_operation(row, "build") for row in commands)
        paired = launch("browser-after-rendering", protected,
                        {"policy": "main-policy", "sources": {"candidate": "candidate-policy"}}, profile="browser")
        assert paired["image_id"] == first["image_id"] and paired["image_tag"] == first["image_tag"]
        assert sum(docker_operation(row, "build") for row in commands) == before
        observed = json.loads((output / "browser-after-rendering/observed.json").read_text())
        assert observed["reused_release"], "The actual container consumer did not verify the promoted release"
        promoted = output / "browser-after-rendering/inputs/candidate-release/scode"
        original = output / "protected-policy/container/rendering/scode-candidate"
        assert promoted.read_bytes() == original.read_bytes()
        print("PASS: actual production dispatcher reuses the same image and canonical-layout release", flush=True)
        original.write_bytes(original.read_bytes() + b"tampered")
        before_run = sum(docker_operation(row, "run") for row in commands)
        try:
            launch("negative-changed-rendering-release", protected,
                   {"policy": "main-policy", "sources": {"candidate": "candidate-policy"}}, profile="browser")
        except RuntimeError as error:
            assert "changed before browser reuse" in str(error), str(error)
        else:
            raise AssertionError("Modified measured release was promoted")
        assert sum(docker_operation(row, "run") for row in commands) == before_run
        candidate = commit("next-candidate", protected)
        before = sum(docker_operation(row, "build") for row in commands)
        launch("different-candidate-does-not-reuse", protected,
               {"policy": "main-policy", "sources": {"candidate": "next-candidate"}}, profile="browser")
        assert sum(docker_operation(row, "build") for row in commands) == before + 1
        assert not json.loads((output / "different-candidate-does-not-reuse/observed.json").read_text())["reused_release"]
        print("PASS: a different committed candidate cannot reuse the earlier release", flush=True)
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
        result = {"status": "PASS", "cases": 8,
            "production_main_and_dispatcher_layout": True, "browser_candidate_only": True,
            "same_invocation_image_and_release_reused": True, "changed_release_refused_before_container": True,
            "changed_candidate_builds_normally": True,
            "independent_same_context_build_keeps_its_own_tag": True,
            "scope": "Real production container launcher, main/dispatcher, command executor, Git commits/archives and Docker builds/runs. The native workload replaces statistical benchmarks and writes only to the output supplied by the dispatcher. Protected/candidate policies and actual ref/tag moves retain frozen inputs. Each build owns a distinct image tag; another same-context build cannot replace it. The browser smoke container receives only the candidate snapshot and executes a staged native Linux binary. A wrong-policy image is rejected inside a real container. Probe executable is the image's Python binary; actual Cargo/source/feature identity is covered by benchmark_targets_acceptance.py. No product benchmark coverage."}
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
