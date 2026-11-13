"""Execute trusted native profiles against the checked-out PR, without secrets."""

import importlib.util
import json
import os
from pathlib import Path

if any(os.environ.get(name) for name in ('GH_TOKEN', 'GITHUB_TOKEN')):
    raise RuntimeError('Native candidate execution must not receive release-download credentials')


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


trusted = Path(__file__).resolve().parents[2]
helper = load("acceptance", trusted / ".github/scripts/local_acceptance.py")
checks = load("checks", trusted / ".github/local-acceptance/checks.py")


def execute(command, cwd, log, env=None):
    # Hosted compilers have no foreground work to protect and use every CPU.
    # Local callers keep the shared host budget; cache restore keeps the
    # publisher's environment.
    hosted = {"CARGO_BUILD_JOBS": str(os.cpu_count() or 1)} if os.environ.get("GITHUB_ACTIONS") == "true" else {}
    return helper.run_command(command, cwd, log, {**(env or {}), **hosted})


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

prepare = getattr(checks, "prepare_native_checks", None)
if prepare:
    checks.run.prepared_native = prepare(native_checks, root, output, execute)

ordered_checks = helper.scheduled_checks(native_checks)
print("CI_NATIVE_EXECUTION_ORDER=" + json.dumps({
    "declared_checks": native_checks, "execution_order": ordered_checks,
    "failure": "First failed profile stops this job; later profiles remain required.",
}), flush=True)
for name in ordered_checks:
    checks.run(name, root, output / name,
               os.environ["ACCEPTANCE_BASE"], execute)
