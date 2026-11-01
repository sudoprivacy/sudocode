"""Exercise shared cache configuration and the real native launcher with Cargo."""
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


def run(output, runner=None):
    output.mkdir(parents=True, exist_ok=False)
    parent = load("cache_fixture_environment", SOURCE / "e2e/local-acceptance/run.py")
    os.environ.clear()
    os.environ.update(parent.FIXTURE_ENV)
    os.environ.pop("CARGO_TARGET_DIR", None)
    helper = load("cache_fixture_executor", SOURCE / ".github/scripts/local_acceptance.py")
    helper.ROOT = SOURCE
    workspace = output / "workspace with spaces"
    candidate = workspace / "candidate"
    source = candidate / "rust"
    producer = workspace / "rust"
    dependency = workspace / "shared"
    for folder in [source, producer, dependency]:
        (folder / "src").mkdir(parents=True)
    (dependency / "Cargo.toml").write_text(
        '[workspace]\n[package]\nname="layout-shared"\nversion="0.0.0"\nedition="2021"\n', encoding="utf-8")
    (dependency / "src/lib.rs").write_text('pub fn number() -> u8 { 42 }\n', encoding="utf-8")
    code = '''use std::path::Path;
fn main() {
    let exe = std::env::current_exe().unwrap().canonicalize().unwrap();
    let expected = std::env::var("EXPECTED_SHARED_TARGET").unwrap();
    assert!(exe.starts_with(Path::new(&expected).canonicalize().unwrap()), "wrong cache target");
    let source = std::env::var("EXPECTED_SOURCE").unwrap();
    assert_eq!(Path::new(env!("CARGO_MANIFEST_DIR")).canonicalize().unwrap(), Path::new(&source).canonicalize().unwrap());
    assert_eq!(layout_shared::number(), 42);
    println!("SOURCE_KIND");
}
'''
    for folder, kind in [(producer, "producer"), (source, "candidate")]:
        (folder / "Cargo.toml").write_text(
            '[workspace]\n[package]\nname="layout-canary"\nversion="0.0.0"\nedition="2021"\n'
            f'[dependencies]\nlayout-shared={{path="{dependency.as_posix()}"}}\n', encoding="utf-8")
        (folder / "src/main.rs").write_text(code.replace("SOURCE_KIND", kind), encoding="utf-8")
        helper.run_command(["cargo", "generate-lockfile", "--offline"], folder, output / f"{kind}-lock.log")
    subprocess.run(["git", "init", "--initial-branch=main", str(candidate)], check=True, capture_output=True)
    configured = helper.git("config", "--get", "localAcceptance.vsdevcmd", check=False).stdout.decode().strip()
    if configured:
        subprocess.run(["git", "-C", str(candidate), "config", "localAcceptance.vsdevcmd", configured], check=True)
    environment = {"GITHUB_WORKSPACE": str(workspace), "GITHUB_ACTIONS": "false"}
    sentinel = output / "github-environment"
    sentinel.write_text("UNCHANGED=1\n", encoding="utf-8")
    layouts = []
    for name, relative in [("publisher", "rust"), ("native", "candidate/rust")]:
        github_output = output / f"{name}-outputs"
        helper.run_command([sys.executable, str(SOURCE / ".github/local-acceptance/cache_layout.py"),
                            "--source", relative, "--github-output"], SOURCE, output / f"{name}-layout.log",
                           {**environment, "GITHUB_OUTPUT": str(github_output), "GITHUB_ENV": str(sentinel)})
        layouts.append(dict(line.split("=", 1) for line in github_output.read_text(encoding="utf-8").splitlines()))
    assert layouts[0]["target"] == layouts[1]["target"]
    assert layouts[0]["workspaces"] == "rust -> target"
    assert layouts[1]["workspaces"] == "candidate/rust -> ../../rust/target"
    assert sentinel.read_text(encoding="utf-8") == "UNCHANGED=1\n", "Configuration changed the pre-restore environment"
    for workflow, count in [("local-acceptance.yml", 1), ("ci-dependency-cache.yml", 2)]:
        text = (SOURCE / ".github/workflows" / workflow).read_text(encoding="utf-8")
        assert text.count("workspaces: ${{ steps.cache_layout.outputs.workspaces }}") == count
    target = Path(layouts[0]["target"])
    helper.run_command(["cargo", "build", "--locked", "--target-dir", str(target)], producer, output / "producer-build.log")
    extension = ".exe" if os.name == "nt" else ""
    binary = target / "debug" / ("layout-canary" + extension)
    helper.run_command([str(binary)], producer, output / "producer-run.log",
                       {"EXPECTED_SHARED_TARGET": str(target), "EXPECTED_SOURCE": str(producer)})
    shared = next((target / "debug/deps").glob("liblayout_shared-*.rlib"))
    before = (hashlib.sha256(shared.read_bytes()).hexdigest(), shared.stat().st_mtime_ns)
    # The production cache discards workspace outputs. Keep the dependency but
    # remove the producer's executable/fingerprint before relocating the source.
    assert target.resolve().is_relative_to(output.resolve())
    helper.run_command(["cargo", "clean", "--package", "layout-canary", "--target-dir", str(target)],
                       producer, output / "trim-workspace.log")
    assert shared.exists(), "Trimming the producer removed its shared dependency"
    trusted = workspace / ".trusted-acceptance"
    for relative in [".github/scripts/local_acceptance.py", ".github/local-acceptance/cache_layout.py"]:
        path = trusted / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(SOURCE / relative, path)
    entry = trusted / ".github/local-acceptance/cross_platform.py"
    shutil.copy2(runner or SOURCE / ".github/local-acceptance/cross_platform.py", entry)
    subprocess.run(["git", "init", "--initial-branch=main", str(trusted)], check=True, capture_output=True)
    if configured:
        subprocess.run(["git", "-C", str(trusted), "config", "localAcceptance.vsdevcmd", configured], check=True)
    (entry.parent / "checks.py").write_text('''import os
import sys
from pathlib import Path
def prepare_native_checks(names, root, output, execute):
    assert names == ["native-workspace"]
    code = ("from pathlib import Path; import os,sys; "
            "Path(sys.argv[1]).write_text('ready'); "
            "print('controlled native prerequisite failure' if os.environ.get('PREPARE_FAIL') else 'ready'); "
            "raise SystemExit(23 if os.environ.get('PREPARE_FAIL') else 0)")
    execute([sys.executable, "-c", code, str(output / "prepare-order")], root, output / "prepare.log")
    return {}
def run(name, root, output, base, execute):
    assert (output.parent / "prepare-order").read_text() == "ready"
    print("RUNNING_NATIVE_PROFILE", flush=True)
    output.mkdir(parents=True)
    commands = [execute(["cargo", "build", "--locked"], root / "rust", output / "build.log")]
    target = Path(os.environ.get("CARGO_TARGET_DIR", root / "rust/target"))
    binary = target / "debug" / ("layout-canary.exe" if os.name == "nt" else "layout-canary")
    commands.append(execute([str(binary)], root, output / "run.log", {"EXPECTED_SOURCE": str(root / "rust")}))
    return commands
''', encoding="utf-8")
    native_env = {**environment, "ACCEPTANCE_NATIVE_CHECKS": "native-workspace", "ACCEPTANCE_BASE": "HEAD",
                  "ACCEPTANCE_OUTPUT": str(output / "native"), "EXPECTED_SHARED_TARGET": str(target)}
    helper.run_command([sys.executable, str(entry)], candidate, output / "native-runner.log", native_env)
    assert (output / "native/native-workspace/run.log").read_text(encoding="utf-8").strip() == "candidate"
    assert before == (hashlib.sha256(shared.read_bytes()).hexdigest(), shared.stat().st_mtime_ns), "Shared dependency recompiled"
    assert not (source / "target").exists(), "The consumer built outside the restored shared target"
    print("Expected negative control: native prerequisite failure must refuse before Cargo/profile execution.", flush=True)
    try:
        helper.run_command([sys.executable, str(entry)], candidate, output / "early-failure.log",
                           {**native_env, "PREPARE_FAIL": "1", "ACCEPTANCE_OUTPUT": str(output / "early-failure")})
    except RuntimeError:
        text = (output / "early-failure.log").read_text(encoding="utf-8")
        assert "controlled native prerequisite failure" in text and "RUNNING_NATIVE_PROFILE" not in text, text
        assert not (output / "early-failure/native-workspace").exists()
    else:
        raise AssertionError("Native launcher ignored failed preparation")
    for name, extra, message in [
        ("wrong-target", {"CARGO_TARGET_DIR": str(workspace / "wrong")}, "differs from the restored cache"),
        ("non-native", {"ACCEPTANCE_NATIVE_CHECKS": "preflight"}, "Only native profiles"),
    ]:
        print(f"Expected negative control: {name} must refuse before compiling.", flush=True)
        try:
            helper.run_command([sys.executable, str(entry)], candidate, output / f"{name}.log",
                               {**native_env, **extra, "ACCEPTANCE_OUTPUT": str(output / name)})
        except RuntimeError:
            assert message in (output / f"{name}.log").read_text(encoding="utf-8")
        else:
            raise AssertionError(f"Native launcher accepted {name}")
    result = {"status": "PASS", "publisher_consumer_target_identical": True,
              "pre_restore_environment_unchanged": True, "actual_candidate_executed": True,
              "shared_dependency_bytes_and_mtime_preserved": True, "wrong_target_and_non_native_refused": True,
              "prerequisite_precedes_Cargo_and_failed_prerequisite_stops_native_profiles": True,
              "scope": "Actual configuration CLI, native launcher, Cargo and native processes. Remote cache restore and product journeys qualify in real CI; this fixture grants no product coverage."}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(result), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--runner", type=Path)
    args = parser.parse_args()
    run(args.output.resolve(), args.runner)
