"""Execute trusted native profiles against the checked-out PR, without secrets."""

import importlib.util
import json
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


def execute(command, cwd, log, env=None):
    # Hosted compilers have no foreground editor to protect. Local callers
    # retain one worker; cache restore keeps the publisher's environment.
    jobs = (os.cpu_count() or 1) if os.environ.get("GITHUB_ACTIONS") == "true" else 1
    return helper.run_command(command, cwd, log, {
        **(env or {}), "CARGO_BUILD_JOBS": str(jobs),
    })


native_checks = os.environ["ACCEPTANCE_NATIVE_CHECKS"].split()
root, output = Path.cwd(), Path(os.environ["ACCEPTANCE_OUTPUT"])
for name in native_checks:
    if not name.startswith("native-"):
        raise RuntimeError("Only native profiles may execute in the complementary CI job")

if os.environ.get("GITHUB_WORKSPACE"):
    layout_module = load("native_cache_layout", trusted / ".github/local-acceptance/cache_layout.py")
    layout = layout_module.resolve(Path(os.environ["GITHUB_WORKSPACE"]), root / "rust")
    configured = os.environ.get("CARGO_TARGET_DIR")
    if configured and Path(configured).resolve() != Path(layout["target"]):
        raise RuntimeError(f"Native Cargo target differs from the restored cache: {configured}; expected {layout['target']}")
    # Cache restore has already finished. Setting this earlier changes the key's
    # environment hash and makes otherwise identical publisher caches miss.
    os.environ["CARGO_TARGET_DIR"] = layout["target"]
    print("CI_CACHE_BINDING=" + json.dumps(layout), flush=True)

if os.name == "nt" and os.environ.get("GITHUB_ACTIONS") == "true":
    # Preserve the former Windows job's protection against mock-server sockets
    # exhausting the default ephemeral range while old sockets are in TIME_WAIT.
    execute(["netsh", "int", "ipv4", "set", "dynamicport", "tcp", "start=10000", "num=55535"],
            root, output / "host-setup/000.log")
    execute(["netsh", "int", "ipv4", "show", "dynamicport", "tcp"],
            root, output / "host-setup/001.log")
    os.environ["RUST_TEST_NOCAPTURE"] = "1"

for name in native_checks:
    checks.run(name, root, output / name,
               os.environ["ACCEPTANCE_BASE"], execute)
