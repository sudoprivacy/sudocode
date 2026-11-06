"""Validate the newly built native artifact before its release job can package it."""
import argparse
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import re
import shutil
import sys


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def accept(root, artifact, output, execute, profiles):
    """Keep identity, original bytes and shared native journeys in one receipt."""
    output.mkdir(parents=True, exist_ok=False)
    source = profiles.committed_git(root, "rev-parse", "HEAD").decode().strip()
    report = {"version": 1, "status": "running", "source": source,
              "started_at": datetime.now(timezone.utc).isoformat(), "commands": []}
    env = {"SCODE_TEST_BACKEND": "mock", "CARGO_BUILD_JOBS": os.environ.get("CARGO_BUILD_JOBS", "1"),
           "CARGO_TARGET_DIR": os.environ.get("CARGO_TARGET_DIR", str(root / "rust/target")),
           "CARGO_PROFILE_DEV_DEBUG": "0", "CARGO_PROFILE_TEST_DEBUG": "0"}

    def command(*arguments, cwd=root, extra=None):
        log = output / f"{len(report['commands']):03d}.log"
        report["commands"].append(execute(arguments, cwd, log, {**env, **(extra or {})}))
        return log

    def cargo(*arguments):
        return command("cargo", *arguments, cwd=root / "rust")

    try:
        if profiles.checkout_link(artifact) or not artifact.is_file():
            raise RuntimeError("Release artifact must be an existing regular native executable, without links")
        original = digest(artifact)
        binary = output / ("scode.exe" if os.name == "nt" else "scode")
        shutil.copy2(artifact, binary)
        if digest(binary) != original:
            raise RuntimeError("Copied release artifact differs from the newly built executable")
        version_log = command(str(binary), "version", "--output-format", "json")
        version = json.loads(version_log.read_text(encoding="utf-8"))
        revision = version.get("git_sha", "")
        if not isinstance(revision, str) or not re.fullmatch(r"[0-9a-f]{7,40}", revision) or not source.startswith(revision):
            raise RuntimeError(f"Release artifact source differs from checkout {source}; rebuild the tag before packaging")
        if Path(version.get("executable_path", "")).resolve() != binary.resolve():
            raise RuntimeError("Release version report does not identify the executed artifact")
        architecture = {"amd64": "x86_64", "arm64": "aarch64"}.get(platform.machine().lower(), platform.machine().lower())
        target = version.get("target", "")
        system = {"Windows": "-windows-", "Darwin": "-apple-darwin", "Linux": "-linux-"}.get(platform.system())
        if not system or not target.startswith(architecture + "-") or system not in target:
            raise RuntimeError(f"Release artifact target {target!r} is not native to {platform.system()}/{architecture}")
        report.update(binary_sha256=original, artifact=str(artifact), binary=str(binary), target=target)
        print("RELEASE_ARTIFACT_GATE: original built bytes and source verified; shared optimized stream/resume, "
              "100ms startup and compiler-context checks must pass before packaging. No second release build.", flush=True)
        profiles.native_release_journeys(root, binary, output, "HEAD", cargo, command, env)
        if digest(artifact) != original or digest(binary) != original:
            raise RuntimeError("Release artifact changed during acceptance")
        report["status"] = "passed"
    except BaseException as error:
        report.update(status="failed", error=str(error))
        raise
    finally:
        report["finished_at"] = datetime.now(timezone.utc).isoformat()
        (output / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--repository", type=Path, default=Path(__file__).resolve().parents[2])
    args = parser.parse_args()
    root = args.repository.resolve()
    scripts = Path(__file__).resolve().parents[1]
    helper = load("release_artifact_commands", scripts / "scripts/local_acceptance.py")
    helper.ROOT = root
    profiles = load("release_artifact_profiles", scripts / "local-acceptance/checks.py")
    source = profiles.committed_git(root, "rev-parse", "HEAD").decode().strip()
    with helper.host_acceptance_lease({}, source):
        report = accept(root, args.binary.absolute(), args.output.resolve(), helper.run_command, profiles)
    print(json.dumps({key: report[key] for key in ["status", "source", "target", "binary_sha256"]}))


if __name__ == "__main__":
    main()
