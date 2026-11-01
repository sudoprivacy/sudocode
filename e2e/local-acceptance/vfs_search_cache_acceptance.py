"""Exercise fixed-driver caching through real Git, Cargo, tar and native processes."""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


SOURCE = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def run(output):
    output.mkdir(parents=True, exist_ok=False)
    parent = load("fixture_environment", SOURCE / "e2e/local-acceptance/run.py")
    os.environ.clear()
    os.environ.update(parent.FIXTURE_ENV)
    helper = load("fixture_executor", SOURCE / ".github/scripts/local_acceptance.py")
    cache = load("fixture_cache", SOURCE / ".github/scripts/vfs_search_cache.py")
    root = output / "work"
    source = root / ".native-vfs"
    source.mkdir(parents=True)
    helper.ROOT = root
    records = []
    environment = {"CARGO_HOME": str(source / ".ci-cache/cargo"),
                   "CARGO_TARGET_DIR": str(source / ".ci-cache/target"), "CARGO_INCREMENTAL": "0",
                   "CARGO_NET_OFFLINE": "true", "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}

    def command(*args, cwd=root, success=True, extra=None):
        log = output / "commands" / f"{len(records):03d}.log"
        try:
            row = helper.run_command(args, cwd, log, {**environment, **(extra or {})})
        except RuntimeError:
            row = json.loads(log.with_suffix(".command.json").read_text())
            records.append(row)
            if success:
                raise
        else:
            records.append(row)
            assert success, ("Expected refusal", args)
        return log

    def write(relative, text):
        path = source / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)

    command("git", "init", "--initial-branch=main", str(source))
    command("git", "config", "user.name", "VFS cache fixture", cwd=source)
    command("git", "config", "user.email", "cache@example.invalid", cwd=source)
    write(".gitignore", "/.ci-cache/\n")
    write("Cargo.toml", '[workspace]\nmembers=["rust/*"]\nresolver="2"\n[profile.dev]\nopt-level=1\n'
          '[profile.dev.package."*"]\nopt-level=2\n')
    write("rust/plugin/Cargo.toml", '[package]\nname="nexus-search-plugin"\nversion="0.0.0"\nedition="2021"\n'
          '[lib]\ncrate-type=["cdylib"]\n')
    write("rust/plugin/src/lib.rs", '#[no_mangle] pub extern "C" fn fixture_value() -> u32 { 42 }\n')
    write("rust/plugin/build.rs", 'fn main() {\n'
          'assert_eq!(std::env::var("CARGO_PKG_NAME").unwrap(),"nexus-search-plugin");\n'
          'for key in ["CARGO_MANIFEST_DIR","CARGO_PKG_NAME","CARGO_PKG_VERSION_MAJOR",'
          '"CARGO_PKG_VERSION_MINOR","CARGO_PKG_VERSION_PATCH","CARGO_PKG_VERSION_PRE"] {\n'
          'println!("cargo:rerun-if-env-changed={key}");\n}\n}\n')
    write("rust/cluster/Cargo.toml", '[package]\nname="nexus-cluster"\nversion="0.0.0"\nedition="2021"\n')
    write("rust/cluster/src/lib.rs", 'pub fn value() -> u32 { 42 }\n')
    write("rust/cluster/tests/user_runtime.rs", '#[test] fn ' + cache.CASE + '() {\n'
          'assert!(std::process::Command::new(env!("CARGO")).args(["build","-p","nexus-search-plugin"]).status().unwrap().success());\n'
          'assert_eq!(nexus_cluster::value(),42);\n'
          'let output=std::process::Command::new(std::env::var("NEXUS_SEARCH_USER_RUNTIME_BINARY").unwrap()).output().unwrap();\n'
          'assert!(output.status.success());\n'
          'assert_eq!(String::from_utf8(output.stdout).unwrap().trim(),"runtime-42");\n}\n')
    command("cargo", "generate-lockfile", "--offline", "--manifest-path", str(source / "Cargo.toml"))
    command("git", "add", ".", cwd=source)
    command("git", "commit", "-m", "Pinned native driver fixture", cwd=source)
    revision = command("git", "rev-parse", "HEAD", cwd=source).read_text().strip()
    (root / "rust").mkdir()
    (root / "rust/Cargo.lock").write_text('source = "git+' + cache.UPSTREAM + '?tag=v0.0.0#' + revision + '"\n')
    entry = SOURCE / ".github/scripts/vfs_search_cache.py"
    runtime = root / "current-runtime"

    def runtime_value(value):
        path = root / "runtime.rs"
        path.write_text(f'fn main() {{ println!("runtime-{value}"); }}\n')
        command("rustc", "--edition=2021", str(path), "-o", str(runtime))

    def invoke(name, label, success=True):
        return command(sys.executable, str(entry), name, "--root", str(root), "--output", str(output / label),
                       success=success, extra={"NEXUS_SEARCH_USER_RUNTIME_BINARY": str(runtime)})

    key = json.loads(command(sys.executable, str(entry), "key", "--root", str(root)).read_text())["key"]
    changed = json.loads(command(sys.executable, str(entry), "key", "--root", str(root),
                                 extra={"CARGO_PROFILE_DEV_OPT_LEVEL": "0"}).read_text())["key"]
    assert key != changed
    runtime_value(42)
    invoke("build", "cold-build")
    invoke("test", "cold-test")
    debug = source / ".ci-cache/target/debug"
    outputs = [debug / "libnexus_search_plugin.so"] + [path for path in (debug / "deps").glob("user_runtime-*")
                                                     if path.is_file() and path.suffix == ""]
    assert len(outputs) == 2, outputs
    before = {str(path.relative_to(source)): (hashlib.sha256(path.read_bytes()).hexdigest(), path.stat().st_mtime_ns)
              for path in outputs}
    archive = root / "cache.tar"
    command("tar", "-cf", str(archive), "-C", str(root), ".native-vfs")
    assert source.resolve() == root / ".native-vfs"
    shutil.rmtree(source)
    command("tar", "-xf", str(archive), "-C", str(root))
    invoke("build", "warm-build")
    warm = json.loads((output / "warm-build/commands.json").read_text())
    assert all(row["compiler_artifacts"] and all(item["fresh"] for item in row["compiler_artifacts"]) for row in warm), warm
    # GNU tar preserves seconds by default. Comparing the exact compiler output
    # bytes plus that stored timestamp proves a normal build did not relink it.
    after = {name: (hashlib.sha256((source / name).read_bytes()).hexdigest(), (source / name).stat().st_mtime_ns)
             for name in before}
    assert all(before[name][0] == after[name][0] and before[name][1] // 10**9 == after[name][1] // 10**9 for name in before)
    invoke("test", "warm-test")
    invoke("build", "after-nested-build")
    after_nested = json.loads((output / "after-nested-build/commands.json").read_text())
    assert all(row["compiler_artifacts"] and all(item["fresh"] for item in row["compiler_artifacts"]) for row in after_nested), after_nested
    original = source / "rust/cluster/src/lib.rs"
    old, stamp = original.read_bytes(), original.stat()
    original.write_bytes(old.replace(b"42", b"43"))
    os.utime(original, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
    refused = invoke("build", "tampered-build", success=False)
    assert "source bytes differ" in refused.read_text()
    original.write_bytes(old)
    os.utime(original, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
    rogue = source / "rust/cluster/src/unknown.rs"
    rogue.write_text("// unexpected input\n")
    refused = invoke("build", "untracked-build", success=False)
    assert "untracked source inputs" in refused.read_text()
    rogue.unlink()
    runtime_value(43)
    refused = invoke("test", "wrong-runtime", success=False)
    assert "runtime-43" in refused.read_text()
    lock = root / "rust/Cargo.lock"
    lock.write_text(lock.read_text().replace(revision, "f" * 40))
    changed_pin = json.loads(command(sys.executable, str(entry), "key", "--root", str(root)).read_text())["key"]
    assert changed_pin != key
    refused = invoke("build", "wrong-pin", success=False)
    assert "revision differs from the current lockfile" in refused.read_text()
    for index, row in enumerate(records):
        path = output / "commands" / f"{index:03d}.log"
        assert hashlib.sha256(path.read_bytes()).hexdigest() == row["log_sha256"]
    result = {"status": "PASS", "revision": revision, "archive_bytes": archive.stat().st_size,
              "normal_build_reuses_restored_native_bytes": True, "current_runtime_journey_runs_again": True,
              "same_mtime_source_tamper_refused": True, "untracked_source_refused": True,
              "wrong_current_runtime_refused": True, "codegen_change_changes_cache_key": True, "commands": records,
              "changed_pin_changes_key_and_refuses_old_checkout": True,
              "nested_cargo_preserves_native_metadata_fingerprints": True,
              "scope": "Real Git/Cargo/tar/native infrastructure fixture; no production VFS journey coverage."}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(f"PASS: pinned-source archive, normal warm Cargo and current runtime refusal; {output}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    run(parser.parse_args().output.resolve())
