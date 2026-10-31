"""Require real Cargo warnings to stop native acceptance before long work."""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys


SOURCE = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def run(output, executor=None):
    output.mkdir(parents=True, exist_ok=False)
    parent = load("lint_fixture_environment", SOURCE / "e2e/local-acceptance/run.py")
    os.environ.clear()
    os.environ.update(parent.FIXTURE_ENV)
    helper = load("lint_fixture_commands", SOURCE / ".github/scripts/local_acceptance.py")
    helper.ROOT = SOURCE
    profiles = load("lint_fixture_profiles", executor or SOURCE / ".github/local-acceptance/checks.py")
    for kind in ["workspace", "selected"]:
        root = output / kind / "source"
        rust = root / "rust"
        cli = rust / "crates/rusty-sudocode-cli"
        acp = rust / "crates/engine-acp"
        (cli / "src").mkdir(parents=True)
        (cli / "tests").mkdir()
        (acp / "src").mkdir(parents=True)
        (rust / "Cargo.toml").write_text(
            '[workspace]\nresolver="2"\nmembers=["crates/rusty-sudocode-cli","crates/engine-acp"]\n', encoding="utf-8")
        (cli / "Cargo.toml").write_text(
            '[package]\nname="rusty-sudocode-cli"\nversion="0.0.0"\nedition="2021"\n'
            '[dependencies]\nengine-acp={path="../engine-acp"}\n', encoding="utf-8")
        (acp / "Cargo.toml").write_text(
            '[package]\nname="engine-acp"\nversion="0.0.0"\nedition="2021"\n[features]\nmailbox=[]\n', encoding="utf-8")
        (acp / "src/lib.rs").write_text('pub fn value() -> u8 {\n    42\n}\n', encoding="utf-8")
        main = cli / "src/main.rs"
        test = cli / "tests/selected.rs"
        main.write_text('fn main() {}\n', encoding="utf-8")
        test.write_text('#[test]\nfn selected() {}\n', encoding="utf-8")
        scripts = root / "scripts"
        scripts.mkdir()
        for name in ["generate_cache_metrics.py", "check_cache_usage_fixtures.py"]:
            (scripts / name).write_text('print("Fixture metadata check")\n', encoding="utf-8")
        sentinel = output / kind / "late-work-started"
        later = root / "e2e/build-metadata"
        later.mkdir(parents=True)
        (later / "run.py").write_text(
            f'from pathlib import Path\nPath({str(sentinel)!r}).touch()\nraise SystemExit(9)\n', encoding="utf-8")
        def git(*args):
            return subprocess.check_output(["git", *args], cwd=root).decode().strip()
        git("init", "--initial-branch=main")
        git("config", "user.name", "Native lint acceptance")
        git("config", "user.email", "lint@example.invalid")
        helper.run_command(["cargo", "generate-lockfile", "--offline"], rust, output / kind / "lock.log")
        git("add", ".")
        git("commit", "-m", "Native lint fixture")
        base = git("rev-parse", "HEAD")
        changed = main if kind == "workspace" else test
        changed.write_text('fn unused() {}\n\n' + changed.read_text(encoding="utf-8"), encoding="utf-8")
        git("add", ".")
        git("commit", "-m", "Introduce a real compiler warning")
        os.environ["CARGO_TARGET_DIR"] = str(output / kind / "target")
        observed = []
        def execute(command, cwd, log, env=None):
            observed.append(list(command))
            return helper.run_command(command, cwd, log, env)
        print(f"Expected negative control: {kind} compiler warning must stop before long work.", flush=True)
        try:
            profiles.run("native-workspace", root, output / kind / "acceptance", base, execute)
        except RuntimeError:
            lint = [command for command in observed if len(command) > 1 and command[1] == "clippy"]
            assert len(lint) == 1 and lint[0][-3:] == ["--", "-D", "warnings"], observed
            logs = '\n'.join(path.read_text(encoding="utf-8", errors="replace")
                             for path in (output / kind / "acceptance").glob("*.log"))
            assert "unused" in logs and "dead_code" in logs, logs[-2000:]
            assert not sentinel.exists(), "Long validation began before the warning was refused"
            assert not any("nextest" in str(command[0]) for command in observed), observed
        else:
            raise AssertionError("A native compiler warning passed the strict lint gate")
    result = {"status": "PASS", "workspace_warning_refused": True, "selected_test_warning_refused": True,
              "long_validation_and_nextest_unrun": True,
              "scope": "Actual native dispatcher, Git, Cargo and compiler warnings. Tiny enforcement fixture; no product journey coverage."}
    (output / "result.json").write_text(json.dumps(result, indent=2) + '\n', encoding="utf-8")
    print(json.dumps(result), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--executor", type=Path)
    args = parser.parse_args()
    run(args.output.resolve(), args.executor)
