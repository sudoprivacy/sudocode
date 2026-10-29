"""Execute trusted native profiles against the checked-out PR, without secrets."""

import importlib.util
import os
from pathlib import Path


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


trusted = Path(__file__).resolve().parents[2]
helper = load("acceptance", trusted / ".github/scripts/local_acceptance.py")
checks = load("checks", trusted / ".github/local-acceptance/checks.py")
for name in os.environ["ACCEPTANCE_NATIVE_CHECKS"].split():
    if not name.startswith("native-"):
        raise RuntimeError("Only native profiles may execute in the complementary CI job")
    checks.run(name, Path.cwd(), Path(os.environ["ACCEPTANCE_OUTPUT"]) / name,
               os.environ["ACCEPTANCE_BASE"], helper.run_command)
