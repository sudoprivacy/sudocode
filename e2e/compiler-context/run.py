"""Run installed-compiler acceptance against the CLI built by this checkout.

The shared native release profile and local callers use source.json and the
same installed-compiler journey. This driver requires no provider key and
builds no Rust code.
"""

import argparse
import hashlib
import json
import re
import subprocess
import sys
from pathlib import Path


def compiler_source(directory, supplied):
    """Resolve the exact public compiler revision without sharing mutable state."""
    pin = json.loads(Path(__file__).with_name("source.json").read_text("utf-8"))
    if pin["repository"] != "https://github.com/sudoprivacy/sudograph.git" or not re.fullmatch("[0-9a-f]{40}", pin["revision"]):
        raise RuntimeError("Invalid compiler source pin")
    source = supplied.resolve() if supplied else directory / "compiler-source"
    if supplied is None:
        source.mkdir()
        for arguments in (["init", "--quiet"], ["remote", "add", "origin", pin["repository"]],
                          ["fetch", "--no-tags", "--depth=1", "origin", pin["revision"]],
                          ["checkout", "--detach", pin["revision"]]):
            subprocess.run(["git", *arguments], cwd=source, check=True)
    actual = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=source, text=True).strip()
    if actual != pin["revision"] or subprocess.check_output(["git", "status", "--porcelain"], cwd=source):
        raise RuntimeError("Compiler acceptance requires the clean pinned source")
    (directory / "compiler-source.json").write_text(json.dumps({**pin, "actual_revision": actual}), encoding="utf-8")
    return source


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=Path, help="Optional existing clean checkout of the exact source.json revision")
    parser.add_argument("--engine", required=True, type=Path)
    parser.add_argument("--directory", required=True, type=Path)
    args = parser.parse_args()
    engine = args.engine.resolve()
    directory = args.directory.resolve()
    directory.mkdir(parents=True, exist_ok=False)
    source = compiler_source(directory, args.source)
    if not (source / "tools/accept_agent_context.py").is_file():
        parser.error("The pinned Sudograph checkout must contain agent context acceptance")
    checksum = hashlib.sha256(engine.read_bytes()).hexdigest()
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
