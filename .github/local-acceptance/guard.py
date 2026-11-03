"""Run the full acceptance decision matrix on a private tracked-source snapshot."""

from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


def main():
    with tempfile.TemporaryDirectory(prefix="scode-contract-source-") as temporary:
        source = Path(temporary) / "source"
        shutil.copytree("/inputs", source, symlinks=True)
        subprocess.run(["git", "init", "--initial-branch=main", str(source)], check=True)
        return subprocess.run([sys.executable, str(source / "e2e/local-acceptance/run.py")], cwd=source).returncode


if __name__ == "__main__":
    raise SystemExit(main())
