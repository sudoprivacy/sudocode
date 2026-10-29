"""Run installed-compiler acceptance against the CLI built by this checkout.

CI pins the Sudograph source revision in rust-ci.yml and reuses its existing
release binary. This driver requires no provider key and builds no Rust code.
"""

import argparse
import hashlib
import subprocess
import sys
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True, type=Path)
    parser.add_argument("--engine", required=True, type=Path)
    parser.add_argument("--directory", required=True, type=Path)
    args = parser.parse_args()
    source = args.source.resolve()
    engine = args.engine.resolve()
    directory = args.directory.resolve()
    if not (source / "tools/accept_agent_context.py").is_file():
        parser.error("The pinned Sudograph checkout must contain agent context acceptance")
    checksum = hashlib.sha256(engine.read_bytes()).hexdigest()
    directory.mkdir(parents=True, exist_ok=True)
    environment = directory / "compiler-python"
    # Never reuse an unknown environment or remove a previous failure's files.
    environment.mkdir()
    subprocess.run([sys.executable, "-m", "venv", str(environment)], check=True)
    python = environment / ("Scripts/python.exe" if sys.platform == "win32" else "bin/python")
    subprocess.run([str(python), "-m", "pip", "install", str(source)], check=True)
    subprocess.run([
        sys.executable, "-m", "tools.accept_agent_context",
        "--engine", str(engine), "--engine-sha256", checksum,
        "--runtime-python", str(python),
        "--evidence-directory", str(directory / "evidence"),
    ], cwd=source, check=True)


if __name__ == "__main__":
    main()
