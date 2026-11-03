"""Exercise fixed-driver caching through real Git, Cargo, tar and native processes."""

import argparse
import ctypes
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys


SOURCE = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def consumer_workflow_script():
    """Execute the required job's actual shell interface in native fixtures."""
    lines = (SOURCE / ".github/workflows/rust-ci.yml").read_text(encoding="utf-8").splitlines()
    name = "      - name: Search authorization and restart under the confined co-host owner"
    starts = [index for index, line in enumerate(lines) if line == name]
    assert len(starts) == 1
    start = starts[0] + 1
    while lines[start] != "        run: |":
        assert not lines[start].startswith("      - ")
        start += 1
    body = []
    for line in lines[start + 1:]:
        if line.strip() and not line.startswith("          "):
            break
        body.append(line[10:])
    script = "set -eu\n" + "\n".join(body) + "\n"
    assert "${{" not in script, "Use the real runner environment in the executable consumer interface"
    return script


def restored_key_workflow_script():
    lines = (SOURCE / ".github/actions/vfs-search-cache/action.yml").read_text().splitlines()
    start = lines.index("    - name: Report restored compiler input key") + 1
    while lines[start] != "      run: |":
        start += 1
    body = []
    for line in lines[start + 1:]:
        if line.strip() and not line.startswith("        "):
            break
        body.append(line[8:])
    return "set -eu\n" + "\n".join(body) + "\n"


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

    dependency = root / "immutable-dependency"
    dependency.mkdir()
    command("git", "init", "--initial-branch=main", str(dependency))
    command("git", "config", "user.name", "Immutable dependency fixture", cwd=dependency)
    command("git", "config", "user.email", "dependency@example.invalid", cwd=dependency)
    (dependency / "Cargo.toml").write_text('[package]\nname="stable-driver-support"\nversion="0.0.1"\nedition="2021"\n')
    (dependency / "src").mkdir()
    (dependency / "src/lib.rs").write_text('pub fn value() -> u32 { 42 }\n')
    command("git", "add", ".", cwd=dependency)
    command("git", "commit", "-m", "Immutable compiled dependency", cwd=dependency)
    dependency_revision = command("git", "rev-parse", "HEAD", cwd=dependency).read_text().strip()
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
    write("rust/cluster/Cargo.toml", '[package]\nname="nexus-cluster"\nversion="0.0.0"\nedition="2021"\n'
          f'[dependencies]\nstable-driver-support={{git="{dependency.as_uri()}",rev="{dependency_revision}"}}\n')
    write("rust/cluster/src/lib.rs", 'pub fn value() -> u32 { stable_driver_support::value() }\n')
    write("rust/cluster/tests/user_runtime.rs", '#[test] fn ' + cache.CASE + '() {\n'
          'assert!(std::process::Command::new(env!("CARGO")).args(["build","-p","nexus-search-plugin"]).status().unwrap().success());\n'
          'assert_eq!(nexus_cluster::value(),42);\n'
          'let output=std::process::Command::new(std::env::var("NEXUS_SEARCH_USER_RUNTIME_BINARY").unwrap()).output().unwrap();\n'
          'assert!(output.status.success());\n'
          'assert_eq!(String::from_utf8(output.stdout).unwrap().trim(),"runtime-42");\n}\n')
    command("cargo", "generate-lockfile", "--manifest-path", str(source / "Cargo.toml"),
            extra={"CARGO_NET_OFFLINE": "false"})
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

    def invoke(name, label, success=True, extra=None):
        return command(sys.executable, str(entry), name, "--root", str(root), "--output", str(output / label),
                       success=success, extra={"NEXUS_SEARCH_USER_RUNTIME_BINARY": str(runtime), **(extra or {})})

    key = json.loads(command(sys.executable, str(entry), "key", "--root", str(root)).read_text())["key"]
    # Captured from the real #1088 Linux consumer. Preserve the exact legacy
    # producer format, not just a second implementation of the new prefix.
    captured = {"pin": "4fd2005d9cdb4ec3aa72983429a4417dbb753ddb",
        "root": "/home/runner/work/sudocode/sudocode/.native-vfs", "recipe": cache.LEGACY_RECIPE,
        "rustc": "rustc 1.99.1 (58cf8f9c3 2026-10-13)\nbinary: rustc\ncommit-hash: 58cf8f9c35b355199727627d07ae34a4e2846d67\n"
                 "commit-date: 2026-10-13\nhost: x86_64-unknown-linux-gnu\nrelease: 1.99.1\nLLVM version: 23.1.1\n",
        "environment": {"CARGO_PROFILE_DEV_DEBUG": "0", "CARGO_PROFILE_TEST_DEBUG": "0",
                        "ImageVersion": "20261026.354.1", "ImageOS": "ubuntu24"}, "cargo_configs": {}}
    assert cache.restore_keys(captured)[0] == "vfs-search-v1-436b0e2c2bbf3e9c3b62998dc222b1aa194fea5cbf6e5f1b11053344f2947fe1"
    github_env = root / "github-env"
    export = restored_key_workflow_script()
    command("bash", "-c", export, extra={"RESTORED_KEY": key, "GITHUB_ENV": str(github_env)})
    assert github_env.read_text() == f"SCODE_VFS_RESTORED_CACHE_KEY={key}\n"
    refused = command("bash", "-c", export, success=False,
                      extra={"RESTORED_KEY": "invalid\nOTHER_VARIABLE=bad", "GITHUB_ENV": str(github_env)})
    assert "Unexpected VFS compiler cache key" in refused.read_text()
    assert github_env.read_text() == f"SCODE_VFS_RESTORED_CACHE_KEY={key}\n"
    changed = json.loads(command(sys.executable, str(entry), "key", "--root", str(root),
                                 extra={"CARGO_PROFILE_DEV_OPT_LEVEL": "0"}).read_text())["key"]
    assert key != changed
    runtime_value(42)
    invoke("build", "cold-build")
    invoke("test", "cold-test")
    refused = invoke("build", "same-pin-wrong-context", success=False,
                     extra={"CARGO_PROFILE_DEV_OPT_LEVEL": "0"})
    assert "Restored VFS compiler context differs" in refused.read_text()
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
    # Run the actual CI shell block, rather than a parallel description of its
    # guard. A producer directory must refuse before a test helper can start.
    consumer = consumer_workflow_script()
    helper_copy = root / ".github/scripts/vfs_search_cache.py"
    helper_copy.parent.mkdir(parents=True)
    shutil.copyfile(entry, helper_copy)
    consumer_temp = output / "workflow-consumer"
    command("bash", "-c", consumer,
            extra={"RUNNER_TEMP": str(consumer_temp), "NEXUS_SEARCH_USER_RUNTIME_BINARY": str(runtime)})
    assert "1 passed; 0 failed" in (consumer_temp / "vfs-search-journey/00.log").read_text()
    forbidden_temp = output / "workflow-redundant-build"
    (forbidden_temp / "vfs-search-build").mkdir(parents=True)
    refused = command("bash", "-c", consumer, success=False,
                      extra={"RUNNER_TEMP": str(forbidden_temp), "NEXUS_SEARCH_USER_RUNTIME_BINARY": str(runtime)})
    assert "redundant pre-test build" in refused.read_text()
    assert not (forbidden_temp / "vfs-search-journey").exists()
    invoke("build", "after-nested-build")
    after_nested = json.loads((output / "after-nested-build/commands.json").read_text())
    assert all(row["compiler_artifacts"] and all(item["fresh"] for item in row["compiler_artifacts"]) for row in after_nested), after_nested
    configuration = source / ".git/config"
    configuration_bytes = configuration.read_bytes()
    outside_config = root / "outside-config"
    outside_config.write_bytes(configuration_bytes + b"# preserve this external file\n")
    outside_bytes = outside_config.read_bytes()
    configuration.unlink()
    configuration.symlink_to(outside_config)
    refused = invoke("build", "linked-git-config", success=False)
    assert "Git configuration must be a regular file" in refused.read_text()
    assert outside_config.read_bytes() == outside_bytes
    configuration.unlink()
    configuration.write_bytes(configuration_bytes)
    original = source / "rust/cluster/src/lib.rs"
    old, stamp = original.read_bytes(), original.stat()
    original.write_bytes(old.replace(b"value() }", b"value() + 1 }"))
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
    # A real source update must preserve only the immutable dependency graph.
    # The actual production helper fetches through a private local Git remote.
    upstream = root / "fixed-upstream.git"
    command("git", "clone", "--bare", str(source), str(upstream))
    author = root / "fixed-author"
    command("git", "clone", str(upstream), str(author))
    command("git", "config", "user.name", "Fixed source update", cwd=author)
    command("git", "config", "user.email", "update@example.invalid", cwd=author)
    for relative in ["rust/plugin/src/lib.rs", "rust/cluster/tests/user_runtime.rs"]:
        path = author / relative
        path.write_text(path.read_text().replace("42", "43"))
    path = author / "rust/cluster/src/lib.rs"
    path.write_text(path.read_text().replace("value() }", "value() + 1 }"))
    command("git", "add", ".", cwd=author)
    command("git", "commit", "-m", "New fixed driver and plugin behaviour", cwd=author)
    next_revision = command("git", "rev-parse", "HEAD", cwd=author).read_text().strip()
    command("git", "push", "origin", "HEAD:main", cwd=author)
    lock = root / "rust/Cargo.lock"
    lock.write_text(lock.read_text().replace(revision, next_revision))
    changed_pin = json.loads(command(sys.executable, str(entry), "key", "--root", str(root)).read_text())["key"]
    assert changed_pin != key
    assert changed_pin.rsplit("-", 1)[0] == key.rsplit("-", 1)[0], "Pin updates lost the compatible dependency prefix"
    migration = root / "migration.py"
    migration.write_text('import importlib.util\n'
        f's=importlib.util.spec_from_file_location("production_cache",{str(entry)!r})\n'
        'm=importlib.util.module_from_spec(s);s.loader.exec_module(m)\n'
        f'm.UPSTREAM={str(upstream)!r}\nm.main()\n')
    identity = source / ".ci-cache/source-identity.json"
    identity_bytes = identity.read_bytes()
    identity.write_text("{broken")
    refused = invoke("build", "malformed-identity", success=False,
                     extra={"GITHUB_ACTIONS": "true", "SCODE_VFS_RESTORED_CACHE_KEY": key})
    assert f"gh cache delete {key} --ref refs/heads/main" in refused.read_text()
    assert f"gh cache delete {changed_pin}" not in refused.read_text()
    identity.unlink()
    refused = invoke("build", "unbound-pin", success=False)
    assert "requires a bound compiler-input identity" in refused.read_text()
    assert command("git", "rev-parse", "HEAD", cwd=source).read_text().strip() == revision
    identity.write_bytes(identity_bytes)
    refused = command(sys.executable, str(migration), "build", "--root", str(root), "--output", str(output / "wrong-context"),
                      success=False, extra={"CARGO_PROFILE_DEV_OPT_LEVEL": "0"})
    assert "Restored VFS compiler context differs" in refused.read_text()
    hook_marker = root / "restored-hook-ran"
    hook = source / ".git/hooks/post-checkout"
    hook.write_text(f'#!/bin/sh\necho hook > {shlex.quote(str(hook_marker))}\n')
    hook.chmod(0o755)
    command(sys.executable, str(migration), "build", "--root", str(root), "--output", str(output / "new-pin-build"))
    assert not hook_marker.exists(), "A restored checkout hook executed during source migration"
    assert command("git", "rev-parse", "HEAD", cwd=source).read_text().strip() == next_revision
    new_build = json.loads((output / "new-pin-build/commands.json").read_text())
    dependency_artifacts = [item for row in new_build for item in row["compiler_artifacts"] if item["name"] == "stable_driver_support"]
    assert dependency_artifacts and all(item["fresh"] for item in dependency_artifacts)
    assert any(not item["fresh"] for row in new_build for item in row["compiler_artifacts"] if item["name"] == "user_runtime")
    invoke("test", "new-pin-current-runtime")
    library = ctypes.CDLL(str(debug / "libnexus_search_plugin.so"))
    library.fixture_value.restype = ctypes.c_uint32
    assert library.fixture_value() == 43, "Restored plugin retained its previous ABI behaviour"
    lock.write_text(lock.read_text().replace(next_revision, "f" * 40))
    refused = command(sys.executable, str(migration), "build", "--root", str(root), "--output", str(output / "wrong-pin"), success=False)
    assert "VFS cache refused before Cargo" in refused.read_text()
    assert command("git", "rev-parse", "HEAD", cwd=source).read_text().strip() == next_revision
    for index, row in enumerate(records):
        path = output / "commands" / f"{index:03d}.log"
        assert hashlib.sha256(path.read_bytes()).hexdigest() == row["log_sha256"]
    result = {"status": "PASS", "revision": revision, "archive_bytes": archive.stat().st_size,
              "normal_build_reuses_restored_native_bytes": True, "current_runtime_journey_runs_again": True,
              "same_mtime_source_tamper_refused": True, "untracked_source_refused": True,
              "wrong_current_runtime_refused": True, "codegen_change_changes_cache_key": True, "commands": records,
              "changed_pin_rebuilds_current_driver_with_fresh_immutable_dependencies": True,
              "unbound_or_incompatible_pin_restore_refused": True, "restored_checkout_hooks_never_execute": True,
              "missing_upstream_revision_refused_without_source_replacement": True,
              "legacy_real_CI_key_preserved": True, "actual_restore_export_and_exact_key_recovery_verified": True,
              "new_pin_native_plugin_abi_verified": True,
              "same_pin_compiler_mismatch_and_linked_git_config_refused": True,
              "nested_cargo_preserves_native_metadata_fingerprints": True,
              "actual_required_workflow_runs_the_native_case": True,
              "actual_required_workflow_refuses_redundant_build_before_test": True,
              "scope": "Real Git/Cargo/tar/native infrastructure fixture; no production VFS journey coverage."}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(f"PASS: pinned-source archive, normal warm Cargo and current runtime refusal; {output}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    run(parser.parse_args().output.resolve())
