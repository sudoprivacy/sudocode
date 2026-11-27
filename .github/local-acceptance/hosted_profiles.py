"""Run the protected non-native profiles in the unprivileged hosted workspace."""

import importlib.util
import json
import os
from pathlib import Path
import shutil


def export_diagnostics(work, artifact):
    """Export command records and reports without descending into build inputs or tool caches."""
    # The mounted Docker evidence directory also holds root-owned compiler
    # argument files, source snapshots and candidate binaries.
    patterns = (
        "*.log", "*.json",
        "container/*/*.log", "container/*/*.json",
        "container/rendering/measurements/report.json",
        "container/rendering/measurements/*/report.json",
        "container/rendering/measurements/*/summary.md",
        "container/browser/smoke/*.json", "container/browser/smoke/*.log",
        "container/browser/viewport-paint/*.json",
        "native-guard/receipt.json",
    )
    for pattern in patterns:
        for source in work.glob(pattern):
            if source.resolve() != source or not source.is_file():
                continue
            target = artifact / source.relative_to(work)
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(source, target)


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
    output = Path(os.environ["ACCEPTANCE_OUTPUT"]).resolve()
    output.mkdir(parents=True, exist_ok=True)
    helper.ROOT = root
    needed = helper.selected(helper.load_policy(policy), helper.changed_paths(base, head), base=base, head=head)
    if not names or names != [name for name in needed if not name.startswith("native-")]:
        raise RuntimeError("Hosted offline inventory differs from the protected source plan")
    # upload-artifact walks its entire input tree, including on failed jobs.
    # Keep execution state outside that tree and export each profile on exit.
    work = output.with_name(output.name + "-work")
    work.mkdir()
    profiles = helper.materialize_profile_module(policy, work)
    profiles.run.execution_refs = (policy, head)
    prepare = getattr(profiles, "prepare_native_checks", None)
    if prepare:
        profiles.run.prepared_native = prepare(names, root, work, helper.run_command)
    for name in names:
        directory = work / name
        try:
            results = profiles.run(name, root, directory, base, helper.run_command)
            (directory / "commands.json").write_text(json.dumps(results, indent=2), encoding="utf-8")
        finally:
            export_diagnostics(directory, output / name)


if __name__ == "__main__":
    main()
