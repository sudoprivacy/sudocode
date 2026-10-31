"""Keep the shared cache publisher and native consumers on identical paths."""

import argparse
import json
import os
from pathlib import Path


def resolve(workspace, source):
    workspace = Path(workspace).resolve()
    source = (workspace / source).resolve()
    target = (workspace / "rust/target").resolve()
    if not source.is_relative_to(workspace) or not target.is_relative_to(workspace):
        raise RuntimeError("Native cache source and target must stay inside GITHUB_WORKSPACE")
    if not (source / "Cargo.toml").is_file():
        raise RuntimeError(f"Native cache source has no Cargo.toml: {source}")
    if target.exists() and not target.is_dir():
        raise RuntimeError(f"Native cache target must be a directory: {target}")
    relative = os.path.relpath(target, source).replace(os.sep, "/")
    return {"source": str(source), "target": str(target),
            "workspaces": f"{source.relative_to(workspace).as_posix()} -> {relative}"}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True)
    parser.add_argument("--github-output", action="store_true")
    args = parser.parse_args()
    layout = resolve(Path(os.environ["GITHUB_WORKSPACE"]), args.source)
    if args.github_output:
        with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as stream:
            for name, value in layout.items():
                stream.write(f"{name}={value}\n")
    # Outputs do not alter Cargo's environment before the action hashes its key.
    print("CI_CACHE_LAYOUT=" + json.dumps(layout), flush=True)


if __name__ == "__main__":
    main()
