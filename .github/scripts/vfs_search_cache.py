"""Reuse fixed VFS compiler inputs while testing the current co-host every time."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import time


UPSTREAM = "https://github.com/nexi-lab/nexus-vfs"
CASE = "signed_search_keeps_the_owner_boundary_and_reopens_committed_indexes"
# The previous producer used these same compiler commands and environment.
# Only its exact current-pin key is eligible for the one-time format migration.
LEGACY_RECIPE = "168f452f188184f9b2e655184aa3faf3208e9e9bf915a9670918ca016382e879"


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
    env = {key: value for key, value in os.environ.items()
           if not key.startswith("CARGO_PKG_") and key not in {"CARGO_MANIFEST_DIR", "CARGO_MANIFEST_PATH", "CARGO_MANIFEST_LINKS"}}
    env.update(CARGO_HOME=str(source / ".ci-cache/cargo"),
               CARGO_TARGET_DIR=str(source / ".ci-cache/target"), CARGO_INCREMENTAL="0")
    metadata = json.loads(subprocess.check_output([
        "cargo", "metadata", "--locked", "--no-deps", "--format-version=1",
        "--manifest-path", str(source / "Cargo.toml"),
    ], cwd=source, env=env))
    packages = [item for item in metadata["packages"] if item["name"] == "nexus-cluster"]
    if len(packages) != 1:
        raise RuntimeError("VFS compiler environment requires exactly one nexus-cluster package")
    package = packages[0]
    version = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)(?:-([^+]+))?(?:\+.+)?", package["version"])
    if version is None:
        raise RuntimeError("VFS cluster package version is not a supported Cargo semver")
    # The original user_runtime case invokes Cargo again from inside its test
    # process. ring records these inherited package variables as fingerprint
    # inputs: unset outer values and Cargo's test values otherwise invalidate
    # the same native dependency repeatedly. Match that real runtime metadata.
    # Cargo still supplies each build script with its OWN package metadata.
    env.update(CARGO_MANIFEST_DIR=str(Path(package["manifest_path"]).parent),
               CARGO_MANIFEST_PATH=package["manifest_path"], CARGO_PKG_VERSION=package["version"],
               CARGO_PKG_VERSION_MAJOR=version[1], CARGO_PKG_VERSION_MINOR=version[2],
               CARGO_PKG_VERSION_PATCH=version[3], CARGO_PKG_VERSION_PRE=version[4] or "",
               CARGO_PKG_NAME=package["name"], CARGO_PKG_AUTHORS=":".join(package["authors"]))
    for name, field in [("DESCRIPTION", "description"), ("HOMEPAGE", "homepage"), ("REPOSITORY", "repository"),
                        ("LICENSE", "license"), ("LICENSE_FILE", "license_file"), ("README", "readme"),
                        ("RUST_VERSION", "rust_version")]:
        env["CARGO_PKG_" + name] = package.get(field) or ""
    if package.get("links"):
        env["CARGO_MANIFEST_LINKS"] = package["links"]
    return env


def compiler_inputs(root):
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
    return inputs


def compiler_context(inputs):
    """Share only dependency artifacts across source pins, never test outcomes."""
    return hashlib.sha256(json.dumps({name: value for name, value in inputs.items() if name != "pin"},
                                    sort_keys=True).encode()).hexdigest()


def cache_key(root):
    inputs = compiler_inputs(root)
    return f"vfs-search-v2-{compiler_context(inputs)}-{inputs['pin']}", inputs


def restore_keys(inputs):
    legacy = {**inputs, "recipe": LEGACY_RECIPE}
    exact_legacy = hashlib.sha256(json.dumps(legacy, sort_keys=True).encode()).hexdigest()
    return ["vfs-search-v1-" + exact_legacy, "vfs-search-v2-" + compiler_context(inputs) + "-"]


def verify(source, revision):
    """Check tracked bytes, including same-mtime edits, before using a restore."""
    if source.is_symlink() or not (source / ".git").is_dir() or (source / ".git").is_symlink():
        raise RuntimeError("VFS cache must contain its own checkout, without linked source or Git roots")
    if (source / ".git/config").is_symlink():
        raise RuntimeError("VFS cache Git configuration must be a regular file")
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


def verify_identity(root, source, current, revision):
    """Only an exact-pin legacy restore can omit the new compiler binding."""
    identity = source / ".ci-cache/source-identity.json"
    if identity.is_symlink():
        raise RuntimeError("VFS compiler-input identity must be a regular file")
    if not identity.exists():
        if current != revision:
            raise RuntimeError("A different VFS pin requires a bound compiler-input identity")
        return identity
    if not identity.is_file():
        raise RuntimeError("VFS compiler-input identity must be a regular file")
    previous = json.loads(identity.read_text())
    expected = compiler_context(compiler_inputs(root))
    if (previous.get("version") != 2 or previous.get("compiler_context") != expected
            or previous.get("pin") != current):
        raise RuntimeError("Restored VFS compiler context differs; no cached source was executed")
    return identity


def update_pin(source, current, revision):
    """Keep external dependencies while rebuilding the fixed workspace at its new pin."""
    verify(source, current)
    # Never run hooks supplied by the restored checkout. The new source still
    # comes from the configured public upstream and must match its exact pin.
    with tempfile.TemporaryDirectory(prefix="vfs-cache-hooks-") as hooks:
        git = ["git", "-C", str(source), "-c", f"core.hooksPath={hooks}"]
        subprocess.run([*git, "fetch", "--depth=1", UPSTREAM, revision], check=True)
        if capture("git", "rev-parse", "FETCH_HEAD", cwd=source).decode().strip() != revision:
            raise RuntimeError("Fetched VFS source differs from the required revision")
        # A restored timestamp can be newer than this host's clock. Discard all
        # old workspace outputs, while retaining registry/Git dependency outputs.
        subprocess.run(["cargo", "clean", "--workspace", "--locked", "--offline",
                        "--manifest-path", str(source / "Cargo.toml")],
                       cwd=source, env=environment(source), check=True)
        subprocess.run([*git, "checkout", "--detach", "FETCH_HEAD"], check=True)
    verify(source, revision)
    print(f"VFS source pin updated {current} -> {revision}; old workspace artifacts removed. "
          "Normal Cargo rebuilds changed inputs and the current-runtime test still runs.", flush=True)


def prepare(root):
    source = root / ".native-vfs"
    revision = pin(root)
    if not source.exists():
        source.mkdir()
        subprocess.run(["git", "init", str(source)], check=True)
        subprocess.run(["git", "-C", str(source), "fetch", "--depth=1", UPSTREAM, revision], check=True)
        subprocess.run(["git", "-C", str(source), "-c", "core.autocrlf=false", "checkout", "--detach", "FETCH_HEAD"], check=True)
    try:
        current = capture("git", "rev-parse", "HEAD", cwd=source).decode().strip()
        identity = verify_identity(root, source, current, revision)
        if current != revision:
            update_pin(source, current, revision)
        count = verify(source, revision)
        if identity.is_symlink():
            raise RuntimeError("VFS compiler-input identity must be a regular file")
        identity.parent.mkdir(parents=True, exist_ok=True)
        identity.write_text(json.dumps({"version": 2, "pin": revision,
            "compiler_context": compiler_context(compiler_inputs(root))}, indent=2) + "\n")
    except (RuntimeError, OSError, ValueError, TypeError, AttributeError, subprocess.CalledProcessError) as error:
        recovery = "Preserve the failed evidence and reacquire the private .native-vfs checkout."
        if os.environ.get("GITHUB_ACTIONS") == "true":
            matched = os.environ.get("SCODE_VFS_RESTORED_CACHE_KEY", "")
            if re.fullmatch(r"vfs-search-v[12]-[a-f0-9]{64}(?:-[a-f0-9]{40})?", matched):
                recovery = (f"Remove only restored cache key {matched} from refs/heads/main with `gh cache delete {matched} "
                            "--ref refs/heads/main --repo sudoprivacy/sudocode`, then rerun this failed job.")
            else:
                recovery = "No restored cache key was identified. " + recovery
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
        fallbacks = restore_keys(inputs)
        print(json.dumps({"key": key, "inputs": inputs, "restore_keys": fallbacks}), flush=True)
        if os.environ.get("GITHUB_OUTPUT"):
            with Path(os.environ["GITHUB_OUTPUT"]).open("a") as stream:
                stream.write(f"key={key}\n")
                stream.write("restore_keys<<VFS_CACHE_RESTORE_KEYS\n" + "\n".join(fallbacks)
                             + "\nVFS_CACHE_RESTORE_KEYS\n")
    else:
        if args.output is None:
            parser.error("build/test requires --output for retained command evidence")
        run(root, args.output.resolve(), test=args.command == "test")


if __name__ == "__main__":
    main()
