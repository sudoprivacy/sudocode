"""Run the full acceptance decision matrix on a private tracked-source snapshot."""

from pathlib import Path
import os
import shutil
import subprocess
import sys
import tempfile


def main():
    if os.environ.get("SCODE_ACCEPTANCE_BUILD_JOBS") != "1":
        raise RuntimeError("Guard image must set SCODE_ACCEPTANCE_BUILD_JOBS=1 before either matrix. "
                           "Rebuild the protected image; keep its CPU, memory and PID limits unchanged.")
    print("GUARD_BUILD_BUDGET: one compiler job through the original executor setting; "
          "Cargo/test commands, Normal test priority, CPU/memory/PID limits and deadlines remain unchanged.",
          flush=True)
    with tempfile.TemporaryDirectory(prefix="scode-contract-source-") as temporary:
        source = Path(temporary) / "source"
        shutil.copytree("/inputs", source, symlinks=True)
        subprocess.run(["git", "init", "--initial-branch=main", str(source)], check=True)
        return subprocess.run([sys.executable, str(source / "e2e/local-acceptance/run.py")], cwd=source).returncode


if __name__ == "__main__":
    raise SystemExit(main())
