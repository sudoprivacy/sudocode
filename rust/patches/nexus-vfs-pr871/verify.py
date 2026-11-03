"""Apply the reviewed dependency patch and verify it without publishing a release."""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("dependency_checkout", type=Path, help="Clean nexus-vfs checkout at the recorded base commit")
parser.add_argument("cargo_args", nargs=argparse.REMAINDER, help="Cargo command; default: test -p runtime --test fs_backend_vfs")
args = parser.parse_args()
bundle = Path(__file__).resolve().parent
workspace = bundle.parents[1]
source = json.loads((bundle / "source.json").read_text(encoding="utf-8"))
patch = bundle / "changes.patch"
if hashlib.sha256(patch.read_bytes()).hexdigest() != source["patch_sha256"]:
    raise SystemExit("Dependency patch checksum mismatch")
checkout = args.dependency_checkout.resolve()
def git(*argv):
    return subprocess.check_output(["git", "-C", str(checkout), *argv]).decode().strip()
if git("rev-parse", "HEAD") != source["base_commit"]:
    raise SystemExit("Dependency checkout is not at the recorded base commit")
if git("status", "--porcelain"):
    raise SystemExit("Dependency checkout must be clean; use a fresh checkout for repeatable verification")
subprocess.run(["git", "-C", str(checkout), "apply", "--check", str(patch)], check=True)
subprocess.run(["git", "-C", str(checkout), "apply", str(patch)], check=True)
packages = {"a2a": "a2a", "backends": "backends", "contracts": "contracts", "kernel": "kernel", "lib": "lib", "managed-agent": "managed_agent", "llm-mount": "llm_mount", "nexus-rebac": "rebac", "transport": "transport", "nexus-cluster": "profiles/cluster"}
config = workspace / "target" / "pr871-verification.toml"
config.parent.mkdir(parents=True, exist_ok=True)
rows = ['[patch."https://github.com/nexi-lab/nexus-vfs"]']
rows += [name + " = { path = " + json.dumps((checkout / "rust" / path).as_posix()) + " }" for name, path in packages.items()]
config.write_text("\n".join(rows) + "\n", encoding="utf-8")
# Local path patching rewrites source entries; do not leave those in the release lockfile.
lock = workspace / "Cargo.lock"
original = lock.read_bytes()
command = args.cargo_args or ["test", "-p", "runtime", "--test", "fs_backend_vfs"]
if command and command[0] == "--": command = command[1:]
try:
    result = subprocess.run(["cargo", *command, "--config", str(config)], cwd=workspace)
finally:
    lock.write_bytes(original)
raise SystemExit(result.returncode)
