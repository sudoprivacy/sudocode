"""Lint the complete feature graph without compiling covered test modules twice."""

import argparse
import importlib.util
import json
from pathlib import Path
import subprocess


ROOT = Path(__file__).resolve().parents[2]


def lint_plan(repository):
    """Reuse the native group inventory; retain every other all-target category."""
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--locked", "--all-features", "--format-version", "1"],
        cwd=repository / "rust"))
    packages = [package for package in metadata["packages"]
                if package["id"] in metadata["workspace_members"]]
    tests = [(package["id"], target) for package in packages for target in package["targets"]
             if target["kind"] == ["test"]]
    names = [target["name"] for _, target in tests]
    scope = {"mode": "all-targets", "integration_targets_before": len(tests),
             "integration_targets_selected": len(tests)}
    target_args = ["--all-targets"]
    if len(names) != len(set(names)):
        scope["reason"] = "overlapping-integration-target-names"
    else:
        spec = importlib.util.spec_from_file_location(
            "queue_clippy_native_inventory", ROOT / ".github/local-acceptance/checks.py")
        recipes = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(recipes)
        grouped, selection = recipes.grouped_workspace_test_targets(repository, metadata)
        if grouped:
            covered = {(group["package"], name) for group in selection["groups"]
                       for name in group["modules"]}
            selected = {grouped[index + 1] for index, argument in enumerate(grouped[:-1])
                        if argument == "--test"}
            # Native execution excludes manual test=false drivers. All-target
            # lint included them, so retain their compilation here as well.
            manual = sorted(target["name"] for package, target in tests
                            if (package, target["name"]) not in covered
                            and target["name"] not in selected)
            target_args = [*grouped, "--examples", "--benches"]
            for name in manual:
                target_args += ["--test", name]
            scope.update(mode="grouped-workspace", groups=selection["groups"],
                         covered_original_targets=len(covered), manual_targets_retained=manual,
                         integration_targets_selected=len(selected) + len(manual))
        else:
            scope["reason"] = "source-without-validated-groups"
    arguments = ["cargo", "clippy", "--workspace", "--locked", "--all-features", *target_args]
    return arguments, scope


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", type=Path, default=ROOT)
    parser.add_argument("--message-format", choices=["json", "human"])
    args = parser.parse_args()
    repository = args.repository.resolve(strict=True)
    command, scope = lint_plan(repository)
    if args.message_format:
        command += ["--message-format=" + args.message_format]
    command += ["--", "-D", "warnings"]
    scope["command"] = command
    print("QUEUE_CLIPPY_SCOPE=" + json.dumps(scope), flush=True)
    print("Queue lint retains all features, library/bin test configurations, examples, benches and manual drivers. "
          "Covered integration modules compile through their validated groups; their original standalone crate "
          "identities are no longer checked again here. No product test case or timeout is removed.", flush=True)
    return subprocess.call(command, cwd=repository / "rust")


if __name__ == "__main__":
    raise SystemExit(main())
