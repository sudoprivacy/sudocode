"""Run the native cohost dispatcher against a real feature/stamp canary."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess


SOURCE = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def run(output, executor=None):
    output.mkdir(parents=True, exist_ok=False)
    parent = load("cohost_fixture_environment", SOURCE / "e2e/local-acceptance/run.py")
    os.environ.clear()
    os.environ.update(parent.FIXTURE_ENV)
    root = output / "source"
    rust = root / "rust"
    (rust / "src").mkdir(parents=True)
    (rust / "Cargo.toml").write_text(
        '[workspace]\n[package]\nname="nexusd-cohost"\nversion="0.0.0"\nedition="2021"\n'
        '[features]\ndaemon=[]\ndriver-ai=[]\negress-gate-presidio=[]\n'
        '[profile.cohost-release]\ninherits="release"\n'
        '[profile.cohost-release-windows]\ninherits="cohost-release"\n', encoding="utf-8")
    code = ('#[cfg(not(all(feature="daemon", feature="driver-ai", feature="egress-gate-presidio")))]\n'
            'compile_error!("All deployed cohost features must stay enabled");\n'
            'fn main() { println!("nexusd-cohost {} debug={}", env!("NEXUSD_BUILD_VERSION"), cfg!(debug_assertions)); }\n')
    main = rust / "src/main.rs"
    main.write_text(code, encoding="utf-8")
    def git(*args):
        return subprocess.check_output(["git", *args], cwd=root).decode().strip()
    git("init", "--initial-branch=main")
    git("config", "user.name", "Cohost profile acceptance")
    git("config", "user.email", "cohost@example.invalid")
    helper = load("cohost_executor", SOURCE / ".github/scripts/local_acceptance.py")
    helper.ROOT = SOURCE
    helper.run_command(["cargo", "generate-lockfile", "--offline"], rust, output / "lock.log")
    git("add", ".")
    git("commit", "-m", "Native cohost feature and version canary")
    profiles = load("cohost_profiles", executor or SOURCE / ".github/local-acceptance/checks.py")
    os.environ["CARGO_TARGET_DIR"] = str(output / "target")
    commands = profiles.run("native-cohost", root, output / "good", "HEAD", helper.run_command)
    version = (output / "good/001.log").read_text(encoding="utf-8")
    assert "debug=true" in version, "Premerge cohost must use the actual dev executable"
    main.write_text(code.replace('env!("NEXUSD_BUILD_VERSION")', '"missing-version"'), encoding="utf-8")
    git("add", ".")
    git("commit", "-m", "Remove the native cohost version stamp")
    try:
        profiles.run("native-cohost", root, output / "wrong-version", "HEAD", helper.run_command)
    except RuntimeError as error:
        assert "Co-host build version stamp was lost" in str(error), str(error)
    else:
        raise AssertionError("A wrong-version cohost executable passed the production profile")
    result = {"status": "PASS", "real_dev_binary_ran": True, "all_deployed_features_required": True,
              "wrong_version_refused": True, "commands": commands,
              "scope": "Actual production dispatcher, Git revision, Cargo feature compilation and native binary version check. Tiny infrastructure canary; real product builds qualify separately on all three native hosts."}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(f"Native cohost profile passed dev/features/stamp/refusal checks: {output}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--executor", type=Path)
    arguments = parser.parse_args()
    run(arguments.output.resolve(), arguments.executor)
