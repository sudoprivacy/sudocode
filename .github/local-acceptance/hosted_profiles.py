"""Run the protected non-native profiles in the unprivileged hosted workspace."""

import importlib.util
import json
import os
from pathlib import Path


def main():
    if any(os.environ.get(name) for name in ("GH_TOKEN", "GITHUB_TOKEN")):
        raise RuntimeError("Hosted candidate execution must not receive GitHub credentials")
    trusted = Path(__file__).resolve().parents[2]
    spec = importlib.util.spec_from_file_location("acceptance", trusted / ".github/scripts/local_acceptance.py")
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    root = Path.cwd()
    policy = helper.value("rev-parse", "HEAD")
    head = helper.value("rev-parse", "HEAD", root=root)
    base = os.environ["ACCEPTANCE_BASE"]
    names = os.environ["ACCEPTANCE_CHECKS"].split()
    output = Path(os.environ["ACCEPTANCE_OUTPUT"])
    output.mkdir(parents=True, exist_ok=True)
    helper.ROOT = root
    needed = helper.selected(helper.load_policy(policy), helper.changed_paths(base, head), base=base, head=head)
    if not names or names != [name for name in needed if not name.startswith("native-")]:
        raise RuntimeError("Hosted offline inventory differs from the protected source plan")
    profiles = helper.materialize_profile_module(policy, output)
    profiles.run.execution_refs = (policy, head)
    prepare = getattr(profiles, "prepare_native_checks", None)
    if prepare:
        profiles.run.prepared_native = prepare(names, root, output, helper.run_command)
    for name in names:
        results = profiles.run(name, root, output / name, base, helper.run_command)
        (output / name / "commands.json").write_text(json.dumps(results, indent=2), encoding="utf-8")


if __name__ == "__main__":
    main()
