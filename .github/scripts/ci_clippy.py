"""Keep full source lint; scope isolated CLI test edits through the native selector."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess


ROOT = Path(__file__).resolve().parents[2]


def lint_plan(repository, base=None):
    """Keep all features and normal targets; narrow only committed isolated tests."""
    metadata = json.loads(subprocess.check_output(
        ["cargo", "metadata", "--no-deps", "--locked", "--all-features", "--format-version", "1"],
        cwd=repository / "rust"))
    packages = [package for package in metadata["packages"]
                if package["id"] in metadata["workspace_members"]]
    tests = [(package["id"], target) for package in packages for target in package["targets"]
             if target["kind"] == ["test"]]
    names = [target["name"] for _, target in tests]
    original_count = sum(target["test"] for _, target in tests)
    scope = {"mode": "all-targets", "integration_targets_declared": len(tests),
             "integration_targets_before": original_count,
             "integration_targets_selected": original_count}
    target_args = ["--all-targets"]
    if len(names) != len(set(names)):
        scope["reason"] = "overlapping-integration-target-names"
    else:
        spec = importlib.util.spec_from_file_location(
            "queue_clippy_native_inventory", ROOT / ".github/local-acceptance/checks.py")
        recipes = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(recipes)
        grouped, selection = recipes.grouped_workspace_test_targets(repository, metadata)
        if not grouped:
            scope["reason"] = "source-without-validated-groups"
        elif not base or not re.fullmatch(r"[0-9a-f]{40}", base):
            scope["reason"] = "missing-or-invalid-queue-base"
        else:
            try:
                dirty = subprocess.check_output(["git", "status", "--porcelain", "--untracked-files=normal"],
                                                cwd=repository)
                changed = recipes.changed_cli_test_targets(repository, base) if not dirty else None
            except (OSError, ValueError, subprocess.CalledProcessError):
                changed = None
                scope["reason"] = "queue-base-unavailable-full-lint-retained"
            eligible = {target["name"] for _, target in tests if target["test"]
                        and Path(target["src_path"]).resolve() == repository / "rust/crates/rusty-sudocode-cli/tests" / (target["name"] + ".rs")}
            if changed and set(changed) <= eligible:
                groups = [group for group in selection["groups"] if set(group["modules"]) & set(changed)]
                selected = sorted(set(changed) | {group["target"] for group in groups})
                target_args = ["--lib", "--bins", "--examples", "--benches"]
                for name in selected:
                    target_args += ["--test", name]
                scope.update(mode="changed-cli-tests", changed_tests=changed, groups=groups,
                             integration_targets_selected=len(selected))
            else:
                scope.setdefault("reason", "ordinary-shared-dirty-or-unregistered-inputs")
    arguments = ["cargo", "clippy", "--workspace", "--locked", "--all-features", *target_args]
    return arguments, scope


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repository", type=Path, default=ROOT)
    parser.add_argument("--message-format", choices=["json", "human"])
    parser.add_argument("--base", default=os.environ.get("SCODE_QUEUE_BASE"))
    args = parser.parse_args()
    repository = args.repository.resolve(strict=True)
    command, scope = lint_plan(repository, args.base)
    if args.message_format:
        command += ["--message-format=" + args.message_format]
    command += ["--", "-D", "warnings"]
    scope["command"] = command
    print("QUEUE_CLIPPY_SCOPE=" + json.dumps(scope), flush=True)
    print("Queue lint retains all features, library/bin test configurations, examples, benches and manual-target exclusions. "
          "Ordinary/shared inputs retain all-targets. Isolated committed CLI test edits select original targets plus "
          "their validated groups; unchanged unrelated integration crates are omitted. No product case or timeout changes.", flush=True)
    return subprocess.call(command, cwd=repository / "rust")


if __name__ == "__main__":
    raise SystemExit(main())
