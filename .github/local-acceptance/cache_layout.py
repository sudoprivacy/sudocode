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


def align_toolchain_cache_key(workspace, source):
    # The cache action reads root-level toolchain files from its own checkout
    # directory, even when Cargo's configured workspace is under candidate/.
    # Mirror the existing candidate pin byte-for-byte so an equivalent publisher
    # and consumer hash the same inputs. Older candidates may lack this file.
    checkout = source.parent
    if checkout == workspace:
        return
    relative = "rust-toolchain.toml"
    candidate = checkout / relative
    if candidate.is_symlink() or not candidate.is_file():
        return
    target = workspace / relative
    contents = candidate.read_bytes()
    try:
        with target.open("xb") as stream:
            stream.write(contents)
    except FileExistsError:
        if target.is_symlink() or not target.is_file() or target.read_bytes() != contents:
            raise RuntimeError("Native cache toolchain pin conflicts with a file at GITHUB_WORKSPACE") from None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True)
    parser.add_argument("--github-output", action="store_true")
    args = parser.parse_args()
    workspace = Path(os.environ["GITHUB_WORKSPACE"]).resolve()
    layout = resolve(workspace, args.source)
    if args.github_output:
        align_toolchain_cache_key(workspace, Path(layout["source"]))
        with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as stream:
            for name, value in layout.items():
                stream.write(f"{name}={value}\n")
    # Outputs do not alter Cargo's environment before the action hashes its key.
    print("CI_CACHE_LAYOUT=" + json.dumps(layout), flush=True)


if __name__ == "__main__":
    main()
