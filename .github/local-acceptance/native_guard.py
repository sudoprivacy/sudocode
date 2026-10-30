"""Run native policy enforcement from one matching protected tool set."""

import argparse
import importlib.util
import json
from pathlib import Path
import sys


SOURCE = Path(__file__).resolve().parents[2]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    repository, output = args.repository.resolve(), args.output.resolve()
    if output == SOURCE or SOURCE in output.parents or output == repository or repository in output.parents:
        raise RuntimeError("Keep native guard outputs outside the source checkout")
    output.mkdir(parents=True, exist_ok=False)
    spec = importlib.util.spec_from_file_location("native_executor", SOURCE / ".github/scripts/local_acceptance.py")
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    helper.ROOT = repository
    # The isolated tool snapshot needs Git discovery for fixture-environment cleanup.
    commands = [helper.run_command(["git", "init", "--initial-branch=main", str(SOURCE)],
                                  output, output / "snapshot-git.log")]
    commands.append(helper.run_command([
        sys.executable, str(SOURCE / "e2e/local-acceptance/resume_acceptance.py"), "--host-smoke",
    ], SOURCE, output / "resume-smoke.log"))
    commands.append(helper.run_command([
        sys.executable, str(SOURCE / ".github/local-acceptance/nextest_guard.py"),
        "--repository", str(repository), "--output", str(output / "nextest-executor"),
    ], SOURCE, output / "nextest.log"))
    (output / "receipt.json").write_text(json.dumps({
        "repository": str(repository), "commands": commands,
    }, indent=2))
    print(f"Native policy guard passed: signed recovery smoke and real nextest enforcement; {output}")


if __name__ == "__main__":
    main()
