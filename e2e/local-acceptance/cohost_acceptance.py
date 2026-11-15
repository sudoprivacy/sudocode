"""Run the native cohost dispatcher against a real feature/stamp canary."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess


SOURCE = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def run(output, executor=None):
    output.mkdir(parents=True, exist_ok=False)
    profiles = load("cohost_profiles", executor or SOURCE / ".github/local-acceptance/checks.py")
    original_home = Path.home()
    protoc = profiles.cohost_protoc()
    parent = load("cohost_fixture_environment", SOURCE / "e2e/local-acceptance/run.py")
    tool = None
    if protoc is not None:
        private = Path(parent.FIXTURE_HOME.name) / protoc.parent.parent.relative_to(original_home)
        shutil.copytree(protoc.parent.parent, private)
        copied = private / "bin" / protoc.name
        digest = hashlib.sha256(protoc.read_bytes()).hexdigest()
        assert hashlib.sha256(copied.read_bytes()).hexdigest() == digest
        tool = {"private_executable": str(copied), "executable_sha256": digest,
                "original_tool_bytes_preserved": True}
    os.environ.clear()
    os.environ.update(parent.FIXTURE_ENV)
    if tool is not None:
        assert profiles.cohost_protoc() == Path(tool["private_executable"])
        # A populated private tool cache must keep this real canary offline.
        os.environ.update(HTTPS_PROXY="http://127.0.0.1:1", https_proxy="http://127.0.0.1:1",
                          NO_PROXY="", no_proxy="")
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
              "isolated_home_tool_cache": tool,
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
