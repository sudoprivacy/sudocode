"""Reuse fixed VFS compiler inputs while testing the current co-host every time."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import time


UPSTREAM = "https://github.com/nexi-lab/nexus-vfs"
CASE = "signed_search_keeps_the_owner_boundary_and_reopens_committed_indexes"


def pin(root):
    """Resolve the same unique public source revision as CI preflight."""
    sources = set(re.findall(r'git\+https://github.com/nexi-lab/nexus-vfs\?tag=[^"#]+#([a-f0-9]{40})',
                             (root / "rust/Cargo.lock").read_text()))
    if len(sources) != 1:
        raise RuntimeError("VFS build cache requires exactly one lockfile revision; fix the pin before retrying")
    return sources.pop()


def capture(*args, cwd):
    return subprocess.check_output(args, cwd=cwd)


def environment(source):
    """Keep external fixture artifacts separate from the candidate's build graph."""
    return {**os.environ, "CARGO_HOME": str(source / ".ci-cache/cargo"),
            "CARGO_TARGET_DIR": str(source / ".ci-cache/target"), "CARGO_INCREMENTAL": "0"}


def cache_key(root):
    source = root / ".native-vfs"
    # Include all explicit codegen overrides, not the candidate revision: the
    # driver source is pinned, whereas its runtime is supplied anew by the job.
    prefixes = ("CARGO_PROFILE_", "CARGO_TARGET_", "CC_", "CXX_", "AR_")
    names = {"RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "RUSTC", "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER",
             "CC", "CXX", "AR", "CFLAGS", "CXXFLAGS", "LDFLAGS", "RUSTUP_TOOLCHAIN", "ImageOS", "ImageVersion"}
    inputs = {"pin": pin(root), "root": str(source), "recipe": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
              "rustc": capture("rustc", "-vV", cwd=root).decode(),
              "environment": {key: value for key, value in os.environ.items()
                              if key in names or key.startswith(prefixes)},
              "cargo_configs": {str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
                                for path in (root / ".cargo/config.toml", root / ".cargo/config") if path.is_file()}}
    digest = hashlib.sha256(json.dumps(inputs, sort_keys=True).encode()).hexdigest()
    return "vfs-search-v1-" + digest, inputs


def verify(source, revision):
    """Check tracked bytes, including same-mtime edits, before using a restore."""
    if source.is_symlink() or not (source / ".git").is_dir() or (source / ".git").is_symlink():
        raise RuntimeError("VFS cache must contain its own checkout, without linked source or Git roots")
    # Cached Git configuration is never an authority for credentials or hooks.
    (source / ".git/config").write_text("[core]\nrepositoryformatversion = 0\nfilemode = true\nbare = false\n")
    if capture("git", "rev-parse", "HEAD", cwd=source).decode().strip() != revision:
        raise RuntimeError("VFS cache revision differs from the current lockfile")
    tracked = set()
    for entry in capture("git", "ls-tree", "-rz", revision, cwd=source).split(b"\0"):
        if not entry:
            continue
        metadata, name = entry.split(b"\t", 1)
        mode, kind, expected = metadata.split()
        if kind != b"blob" or mode not in {b"100644", b"100755"}:
            raise RuntimeError("VFS cache source layout changed; qualify its new input types first")
        relative = name.decode()
        path = source / relative
        if path.is_symlink() or not path.is_file():
            raise RuntimeError(f"VFS cache has a missing or linked source: {relative}")
        contents = path.read_bytes()
        actual = hashlib.sha1(b"blob " + str(len(contents)).encode() + b"\0" + contents).hexdigest().encode()
        if actual != expected:
            raise RuntimeError(f"VFS cache source bytes differ: {relative}")
        tracked.add(relative)
    found = set()
    for directory, children, files in os.walk(source, followlinks=False):
        directory = Path(directory)
        if directory == source:
            children[:] = [name for name in children if name not in {".git", ".ci-cache"}]
        if any((directory / name).is_symlink() for name in children + files):
            raise RuntimeError("VFS cache contains a linked source input")
        found.update((directory / name).relative_to(source).as_posix() for name in files)
    if found != tracked:
        raise RuntimeError(f"VFS cache contains untracked source inputs: {sorted(found - tracked)[:5]}; "
                           f"missing tracked inputs: {sorted(tracked - found)[:5]}")
    for relative in [".ci-cache", ".ci-cache/cargo", ".ci-cache/target"]:
        if (source / relative).is_symlink():
            raise RuntimeError("VFS cache build paths must stay inside the fixed checkout")
    if any((source / ".ci-cache/cargo" / name).exists() for name in ["credentials", "credentials.toml"]):
        raise RuntimeError("VFS compiler cache must not contain Cargo credentials")
    return len(tracked)


def prepare(root):
    source = root / ".native-vfs"
    revision = pin(root)
    if not source.exists():
        source.mkdir()
        subprocess.run(["git", "init", str(source)], check=True)
        subprocess.run(["git", "-C", str(source), "fetch", "--depth=1", UPSTREAM, revision], check=True)
        subprocess.run(["git", "-C", str(source), "-c", "core.autocrlf=false", "checkout", "--detach", "FETCH_HEAD"], check=True)
    try:
        count = verify(source, revision)
    except (RuntimeError, OSError, subprocess.CalledProcessError) as error:
        recovery = "Preserve the failed evidence and reacquire the private .native-vfs checkout."
        if os.environ.get("GITHUB_ACTIONS") == "true":
            key, _ = cache_key(root)
            recovery = (f"Remove only cache key {key} from refs/heads/main with `gh cache delete {key} "
                        "--ref refs/heads/main --repo sudoprivacy/sudocode`, then rerun this failed job.")
        raise RuntimeError(f"VFS cache refused before Cargo: {error}. {recovery}") from error
    print(f"VFS compiler cache: verified {count} tracked files at {revision}. "
          "Normal Cargo builds and the current co-host journey remain required.", flush=True)
    return source


def run(root, output, test=False):
    source = prepare(root)
    output.mkdir(parents=True, exist_ok=False)
    env = environment(source)
    commands = [["cargo", "build", "--locked", "--manifest-path", str(source / "Cargo.toml"), "-p", "nexus-search-plugin"],
                ["cargo", "test", "--locked", "--manifest-path", str(source / "Cargo.toml"),
                 "-p", "nexus-cluster", "--all-features", "--test", "user_runtime", "--no-run"]]
    runtime_hash = None
    if test:
        runtime = Path(os.environ.get("NEXUS_SEARCH_USER_RUNTIME_BINARY", ""))
        if not runtime.is_absolute() or not runtime.is_file() or source in runtime.resolve().parents:
            raise RuntimeError("Search acceptance requires an explicit current co-host outside the fixed driver cache")
        commands = [commands[1][:-1] + [CASE, "--", "--nocapture"]]
        runtime_hash = hashlib.sha256(runtime.read_bytes()).hexdigest()
    else:
        commands = [command + ["--message-format=json-render-diagnostics"] for command in commands]
    (output / "context.json").write_text(json.dumps({
        "source_revision": pin(root), "recipe_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "current_runtime_sha256": runtime_hash, "test_required_now": test,
    }, indent=2) + "\n")
    records = []
    for index, command in enumerate(commands):
        start = time.monotonic()
        path = output / f"{index:02d}.log"
        print("VFS normal Cargo command: " + " ".join(command), flush=True)
        with path.open("wb") as stream:
            result = subprocess.run(command, cwd=root, env=env, stdout=stream, stderr=subprocess.STDOUT)
        row = {"command": command, "exit_code": result.returncode, "seconds": round(time.monotonic() - start, 3),
               "log_sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
        artifacts = []
        for line in ([] if test else path.read_text(errors="replace").splitlines()):
            if line.startswith("{"):
                message = json.loads(line)
                if message.get("reason") == "compiler-artifact":
                    artifacts.append({"name": message["target"]["name"], "fresh": message["fresh"]})
        row["compiler_artifacts"] = artifacts
        records.append(row)
        (output / "commands.json").write_text(json.dumps(records, indent=2) + "\n")
        print(path.read_text(errors="replace")[-16000:], flush=True)
        if result.returncode:
            raise RuntimeError(f"VFS Cargo command failed with {result.returncode}; evidence: {path}. "
                               "No earlier passing result authorizes this run.")
    print("VFS_SEARCH_RESULT=" + json.dumps({"test_ran": test, "commands": records}), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["key", "build", "test"])
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()
    root = args.root.resolve()
    if args.command == "key":
        key, inputs = cache_key(root)
        print(json.dumps({"key": key, "inputs": inputs}), flush=True)
        if os.environ.get("GITHUB_OUTPUT"):
            with Path(os.environ["GITHUB_OUTPUT"]).open("a") as stream:
                stream.write(f"key={key}\n")
    else:
        if args.output is None:
            parser.error("build/test requires --output for retained command evidence")
        run(root, args.output.resolve(), test=args.command == "test")


if __name__ == "__main__":
    main()
