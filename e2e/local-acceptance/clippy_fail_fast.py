"""Exercise the CI lint command with real independent Cargo packages."""

import argparse
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import time

import psutil


SOURCE = Path(__file__).resolve().parents[2]


def run(output):
    output.mkdir(parents=True, exist_ok=False)
    workflow = (SOURCE / ".github/workflows/rust-ci.yml").read_text(encoding="utf-8")
    job = workflow.split("  clippy-workspace:\n", 1)[1].split("\n  integrated-acceptance:", 1)[0]
    commands = [shlex.split(line.strip()[5:]) for line in job.splitlines()
                if line.strip().startswith("run: cargo clippy ")]
    assert len(commands) == 1, "CI lint must have one explicit Cargo command; update this fixture if its interface changes"
    command = commands[0]
    assert command[:2] == ["cargo", "clippy"]
    assert {"--workspace", "--all-targets", "--all-features"}.issubset(command)
    assert command[-3:] == ["--", "-D", "warnings"]
    assert "--keep-going" not in command, "CI must stop after its first lint error"
    project = output / "project"
    project.mkdir()
    (project / "Cargo.toml").write_text('[workspace]\nmembers=["first", "second", "third"]\nresolver="2"\n')
    for name in ["first", "second", "third"]:
        package = project / name
        (package / "src").mkdir(parents=True)
        (package / "Cargo.toml").write_text(f'[package]\nname="lint-{name}"\nversion="0.0.0"\nedition="2021"\n'
            '[features]\nextra=[]\n')
        (package / "src/lib.rs").write_text('#[cfg(feature="extra")]\npub fn feature_path() { let unused_value = 42; }\n')
    environment = dict(os.environ, CARGO_BUILD_JOBS="1", CARGO_INCREMENTAL="0",
                       CARGO_PROFILE_DEV_DEBUG="0", CARGO_PROFILE_TEST_DEBUG="0", CARGO_TERM_COLOR="always")
    # Lint compilation runs no product/test executable. Keep this fixture's
    # Python assertions Normal while its Cargo/compiler process stays below it.
    def invoke(label, arguments, target):
        arguments = [*arguments[:2], "--target-dir", str(target), "--message-format=json", *arguments[2:]]
        if os.name != "nt":
            arguments = ["nice", "-n", "10", *arguments]
        started = time.monotonic()
        stdout, stderr = output / (label + ".stdout.log"), output / (label + ".stderr.log")
        priorities = {}
        with stdout.open("wb") as out, stderr.open("wb") as err:
            process = subprocess.Popen(arguments, cwd=project, env=environment, stdout=out, stderr=err,
                creationflags=subprocess.BELOW_NORMAL_PRIORITY_CLASS if os.name == "nt" else 0)
            while process.poll() is None:
                try:
                    for child in [psutil.Process(process.pid), *psutil.Process(process.pid).children(recursive=True)]:
                        try:
                            name = Path(child.exe()).name.lower()
                            if name in {"rustc", "rustc.exe", "clippy-driver", "clippy-driver.exe"}:
                                priorities[f"{child.pid}:{name}"] = child.nice()
                        except (psutil.NoSuchProcess, psutil.AccessDenied):
                            continue
                except (psutil.NoSuchProcess, psutil.AccessDenied):
                    pass
                time.sleep(0.01)
        assert priorities, "Did not observe the actual lint compiler; inspect the retained process logs"
        expected = psutil.BELOW_NORMAL_PRIORITY_CLASS if os.name == "nt" else 10
        assert all(value == expected for value in priorities.values()), priorities
        messages = [json.loads(line) for line in stdout.read_text(encoding="utf-8").splitlines() if line.startswith("{")]
        errors = [row for row in messages if row.get("reason") == "compiler-message"
                  and row["message"]["level"] == "error" and row["message"].get("code", {}).get("code") == "unused_variables"]
        artifacts = [row for row in messages if row.get("reason") == "compiler-artifact"]
        return {"command": arguments, "exit_code": process.returncode, "compiler_priorities": priorities,
                "elapsed_seconds": round(time.monotonic() - started, 3),
                "failed_packages": sorted({row["package_id"] for row in errors}),
                "artifacts": artifacts}

    fast = invoke("first-failure", command, output / "fast-target")
    previous = [*command[:2], "--keep-going", *command[2:]]
    continuing = invoke("previous-keep-going", previous, output / "previous-target")
    assert fast["exit_code"] != 0 and len(fast["failed_packages"]) == 1, fast
    assert continuing["exit_code"] != 0 and len(continuing["failed_packages"]) == 3, continuing
    # The warning exists only under the extra feature: the actual negative
    # proves all-features and -D warnings remain active in the selected command.
    for package in project.iterdir():
        source = package / "src/lib.rs"
        if source.is_file():
            source.write_text(source.read_text().replace("let unused_value", "let _unused_value"))
    passing = invoke("clean-workspace", command, output / "fast-target")
    assert passing["exit_code"] == 0, passing
    artifacts = passing["artifacts"]
    assert len({row["package_id"] for row in artifacts}) == 3, artifacts
    assert all("extra" in row["features"] for row in artifacts), artifacts
    assert {row["profile"]["test"] for row in artifacts} == {False, True}, artifacts
    result = {"status": "PASS", "first_failure": fast, "previous_keep_going": continuing,
              "clean_workspace": passing,
              "scope": "Actual CI command and Cargo/Clippy processes on three tiny independent feature-gated packages. Successful all-target/all-feature lint coverage is retained. This is infrastructure acceptance, not product PTY or production latency evidence."}
    (output / "result.json").write_text(json.dumps(result, indent=2), encoding="utf-8")
    print(json.dumps({"status": "PASS", "failed_packages_before": 3, "failed_packages_after": 1,
                      "passing_packages": 3, "evidence": str(output)}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    run(parser.parse_args().output.resolve())
