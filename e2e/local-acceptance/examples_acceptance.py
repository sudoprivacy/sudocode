"""Exercise the production compile-only example profile with real Cargo processes."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import sys


SOURCE = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def run(output):
    output.mkdir(parents=True, exist_ok=False)
    root = output / "source"
    rust = root / "rust"
    (rust / "src").mkdir(parents=True)
    (rust / "examples").mkdir()
    (rust / "Cargo.toml").write_text(
        '[workspace]\nmembers=["crates/rusty-sudocode-cli"]\nresolver="2"\n'
        '[package]\nname="engine-acp"\nversion="0.0.0"\nedition="2021"\n'
        '[features]\nmailbox=[]\n', encoding="utf-8")
    (rust / "src/lib.rs").write_text('pub fn marker() -> &\'static str { "example-retained" }\n', encoding="utf-8")
    example = rust / "examples/canary.rs"
    # Profile compilation must not execute this program's observable side effect.
    example.write_text('fn main() {\n    std::fs::write("example-ran", "ran").unwrap();\n'
                       '    println!("{}", engine_acp::marker());\n}\n', encoding="utf-8")
    helper = load("example_executor", SOURCE / ".github/scripts/local_acceptance.py")
    profiles = load("example_profiles", SOURCE / ".github/local-acceptance/checks.py")
    helper.ROOT = SOURCE
    cli = rust / "crates/rusty-sudocode-cli"
    (cli / "src").mkdir(parents=True)
    (cli / "tests/acceptance_groups").mkdir(parents=True)
    (cli / "Cargo.toml").write_text(
        '[package]\nname="rusty-sudocode-cli"\nversion="0.0.0"\nedition="2021"\n'
        '[dependencies]\nengine-acp={path="../.."}\n'
        '[[test]]\nname="acceptance_cli_01"\npath="tests/acceptance_groups/acceptance_cli_01.rs"\ntest=false\n',
        encoding="utf-8")
    (cli / "src/main.rs").write_text('fn main() { println!("native-cli-42"); }\n', encoding="utf-8")
    test = cli / "tests/native_driver.rs"
    test.write_text('''#[test]
fn actual_cli_process() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_rusty-sudocode-cli")).output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "native-cli-42");
}
''', encoding="utf-8")
    (cli / "tests/acceptance_groups/acceptance_cli_01.rs").write_text(
        profiles.test_group_source(["native_driver"]), encoding="utf-8")
    for relative in [".github/scripts/local_acceptance.py", "spec/local_acceptance.json"]:
        destination = root / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes((SOURCE / relative).read_bytes())
    commands = [helper.run_command(["cargo", "generate-lockfile", "--offline"], rust, output / "lock.log")]
    previous = os.environ.get("CARGO_TARGET_DIR")
    os.environ["CARGO_TARGET_DIR"] = str(output / "target")
    try:
        commands.append(helper.run_command(["cargo", "fmt", "--all"], rust, output / "format.log"))
        for index, arguments in enumerate([
            ["init", "--initial-branch=main"], ["config", "user.name", "Example scope acceptance"],
            ["config", "user.email", "examples@example.invalid"], ["config", "core.autocrlf", "false"],
            ["add", "."], ["commit", "-m", "Baseline example and native inputs"],
        ]):
            commands.append(helper.run_command(["git", *arguments], root, output / f"git-{index}.log"))
        base = profiles.committed_git(root, "rev-parse", "HEAD").decode().strip()
        commands += profiles.run("native-examples", root, output / "good", "HEAD", helper.run_command)
        assert not (rust / "example-ran").exists(), "Compile-only profile ran an example entry point"
        binary = output / "target/debug/examples" / ("canary.exe" if os.name == "nt" else "canary")
        assert binary.is_file(), "Native profile omitted the normal example binary"
        observed = output / "actual-example.log"
        commands.append(helper.run_command([str(binary)], rust, observed))
        assert observed.read_text(encoding="utf-8").strip() == "example-retained"
        assert (rust / "example-ran").read_text() == "ran"
        (rust / "example-ran").unlink()
        binary_sha = hashlib.sha256(binary.read_bytes()).hexdigest()
        test.write_text(test.read_text(encoding="utf-8") + "// Change only the native acceptance module.\n", encoding="utf-8")
        for index, arguments in enumerate([["add", "rust"], ["commit", "-m", "Change an isolated native test"]]):
            commands.append(helper.run_command(["git", *arguments], root, output / f"test-git-{index}.log"))
        plan_log = output / "isolated-plan.log"
        commands.append(helper.run_command([
            sys.executable, str(root / ".github/scripts/local_acceptance.py"), "plan",
            "--base", base, "--policy-ref", base, "--host", "windows", "--architecture", "AMD64",
        ], root, plan_log))
        plan = json.loads(next(line.split("=", 1)[1] for line in plan_log.read_text(encoding="utf-8").splitlines()
                               if line.startswith("ACCEPTANCE_PLAN=")))
        assert "native-examples" not in plan["local_checks"], plan
        assert "native-workspace" in plan["local_checks"], plan
        assert profiles.changed_cli_test_targets(root, base) == ["native_driver"]
        commands += profiles.run("native-workspace", root, output / "isolated-native", base, helper.run_command)
        fresh_log = output / "unchanged-example-build.log"
        commands.append(helper.run_command([
            "cargo", "build", "--workspace", "--locked", "--features", "engine-acp/mailbox",
            "--examples", "--message-format=json",
        ], rust, fresh_log, {"CARGO_TARGET_DIR": str(output / "target")}))
        artifacts = [json.loads(line) for line in fresh_log.read_text(encoding="utf-8").splitlines() if line.startswith("{")]
        examples = [entry for entry in artifacts if entry.get("reason") == "compiler-artifact"
                    and entry["target"]["kind"] == ["example"]]
        assert all(Path(entry["executable"]).resolve().is_relative_to((output / "target").resolve())
                   for entry in examples), examples
        assert len(examples) == 1 and examples[0]["fresh"], examples
        assert hashlib.sha256(binary.read_bytes()).hexdigest() == binary_sha
        test.chmod(test.stat().st_mode | 0o111)
        commands.append(helper.run_command([
            "git", "update-index", "--chmod=+x", "rust/crates/rusty-sudocode-cli/tests/native_driver.rs",
        ], root, output / "mode-index.log"))
        commands.append(helper.run_command([
            "git", "commit", "-m", "Unknown native target mode retains complete workspace",
        ], root, output / "mode-commit.log"))
        mode_log = output / "mode-plan.log"
        commands.append(helper.run_command([
            sys.executable, str(root / ".github/scripts/local_acceptance.py"), "plan",
            "--base", base, "--policy-ref", base, "--host", "windows", "--architecture", "AMD64",
        ], root, mode_log))
        mode_text = mode_log.read_text(encoding="utf-8")
        mode_plan = json.loads(next(line.split("=", 1)[1] for line in mode_text.splitlines()
                                   if line.startswith("ACCEPTANCE_PLAN=")))
        mode_scope = json.loads(next(line.split("=", 1)[1] for line in mode_text.splitlines()
                                    if line.startswith("EXAMPLE_BUILD_SCOPE=")))
        assert "native-examples" in mode_plan["local_checks"], mode_plan
        assert mode_scope["separate_profile"] and not mode_scope["compiled_in_workspace"], mode_scope
        # Exercise the full production workspace entry, not only its standalone
        # helper: examples must still link before later native work begins.
        metadata = root / "e2e/build-metadata/run.py"
        metadata.parent.mkdir(parents=True)
        later = rust / "later-native-work"
        metadata.write_text('from pathlib import Path\nPath("rust/later-native-work").write_text("started")\n',
                            encoding="utf-8")
        vendor = rust / "vendor/codex-mermaid"
        (vendor / "src").mkdir(parents=True)
        (vendor / "Cargo.toml").write_text(
            '[workspace]\n[package]\nname="example-vendor-canary"\nversion="0.0.0"\nedition="2021"\n',
            encoding="utf-8")
        (vendor / "src/lib.rs").write_text('pub fn marker() -> bool {\n    true\n}\n', encoding="utf-8")
        commands.append(helper.run_command([
            "cargo", "generate-lockfile", "--offline", "--manifest-path", str(vendor / "Cargo.toml"),
        ], rust, output / "vendor-lock.log"))
        (cli / "src/main.rs").write_text((cli / "src/main.rs").read_text() + "// Full source acceptance.\n",
                                         encoding="utf-8")
        for index, arguments in enumerate([["add", "."], ["commit", "-m", "Require complete native workspace"]]):
            commands.append(helper.run_command(["git", *arguments], root, output / f"full-git-{index}.log"))
        full_plan_log = output / "full-plan.log"
        commands.append(helper.run_command([
            sys.executable, str(root / ".github/scripts/local_acceptance.py"), "plan",
            "--base", base, "--policy-ref", base, "--host", "windows", "--architecture", "AMD64",
        ], root, full_plan_log))
        full_text = full_plan_log.read_text(encoding="utf-8")
        full_plan = json.loads(next(line.split("=", 1)[1] for line in full_text.splitlines()
                                   if line.startswith("ACCEPTANCE_PLAN=")))
        full_scope = json.loads(next(line.split("=", 1)[1] for line in full_text.splitlines()
                                    if line.startswith("EXAMPLE_BUILD_SCOPE=")))
        assert "native-workspace" in full_plan["local_checks"] and "native-examples" not in full_plan["local_checks"], full_plan
        assert full_scope["compiled_in_workspace"] and not full_scope["separate_profile"], full_scope
        full_commands = profiles.run("native-workspace", root, output / "full-native", base, helper.run_command)
        commands += full_commands
        original_arguments = ["build", "--workspace", "--locked", "--features", "engine-acp/mailbox", "--examples"]
        built = [row for row in full_commands if row["command"][1:] == original_arguments]
        assert len(built) == 1 and binary.is_file(), full_commands
        assert later.is_file() and not (rust / "example-ran").exists()
        # A policy with a distinct example host cannot be covered by a workspace
        # receipt from another platform. Its original standalone job must stay.
        policy_path = root / "spec/local_acceptance.json"
        original_policy = policy_path.read_bytes()
        policy = json.loads(original_policy)
        policy["checks"]["native-examples"]["targets"] = ["windows", "linux-arm64"]
        policy_path.write_text(json.dumps(policy), encoding="utf-8")
        for index, arguments in enumerate([["add", "."], ["commit", "-m", "Retain distinct example host"]]):
            commands.append(helper.run_command(["git", *arguments], root, output / f"host-git-{index}.log"))
        distinct_log = output / "distinct-host-plan.log"
        commands.append(helper.run_command([
            sys.executable, str(root / ".github/scripts/local_acceptance.py"), "plan",
            "--base", base, "--policy-ref", "HEAD", "--host", "windows", "--architecture", "AMD64",
        ], root, distinct_log))
        distinct_plan = json.loads(next(line.split("=", 1)[1]
                                       for line in distinct_log.read_text(encoding="utf-8").splitlines()
                                       if line.startswith("ACCEPTANCE_PLAN=")))
        assert any("native-examples" in job["checks"] for job in distinct_plan["ci"]), distinct_plan
        policy_path.write_bytes(original_policy)
        later.unlink()
        example.write_text('#[link(name = "SCODE_MISSING_EXAMPLE_LIBRARY")]\n'
                           'unsafe extern "C" { fn missing_example_symbol(); }\n'
                           'fn main() { unsafe { missing_example_symbol(); } }\n', encoding="utf-8")
        commands.append(helper.run_command(["cargo", "fmt", "--all"], rust, output / "link-failure-format.log"))
        try:
            profiles.run("native-workspace", root, output / "broken-full-native", base, helper.run_command)
        except RuntimeError:
            full_logs = sorted((output / "broken-full-native").glob("*.command.json"))
            linking = [json.loads(path.read_text()) for path in full_logs
                       if json.loads(path.read_text())["command"][1:] == original_arguments]
            assert len(linking) == 1 and linking[0]["exit_code"] != 0, full_logs
            assert not later.exists(), "Long native work began before example link failure"
        else:
            raise AssertionError("Full native acceptance omitted example linking")
        example.write_text('compile_error!("BROKEN_COMPILE_ONLY_EXAMPLE");\nfn main() {}\n', encoding="utf-8")
        try:
            profiles.run("native-examples", root, output / "broken", "HEAD", helper.run_command)
        except RuntimeError:
            failed = json.loads((output / "broken/000.command.json").read_text())
            assert failed["exit_code"] != 0
            assert "BROKEN_COMPILE_ONLY_EXAMPLE" in (output / "broken/000.log").read_text(encoding="utf-8")
        else:
            raise AssertionError("The production profile accepted a broken compile-only example")
    finally:
        if previous is None:
            os.environ.pop("CARGO_TARGET_DIR", None)
        else:
            os.environ["CARGO_TARGET_DIR"] = previous
    result = {"status": "PASS", "normal_example_built": True, "profile_did_not_run_example": True,
              "actual_example_process_passed": True, "broken_example_refused": True,
              "isolated_test_plan": plan, "isolated_native_process_retained": True,
              "unchanged_example_fresh_and_same_hash": True,
              "full_workspace_examples": {"plan": full_plan, "scope": full_scope,
                                           "nonordinary_target_mode_retains_standalone_examples": True,
                                           "original_compile_command_executed_once": True,
                                           "example_entry_not_run": True,
                                           "link_failure_before_later_work": True,
                                           "different_host_retained": distinct_plan},
              "commands": commands, "expected_failure": failed,
              "scope": "Real production dispatcher, Cargo compilation/linking and native example execution. Tiny infrastructure fixture; no product/provider coverage credit."}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(f"Native example profile passed normal-link/compile-only/runtime-canary/broken-example checks: {output}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    run(parser.parse_args().output.resolve())
