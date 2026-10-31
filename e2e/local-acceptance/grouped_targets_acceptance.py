"""Compare real Cargo/nextest inventories, processes and opt-in target scope."""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys


SOURCE = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def run(output, repository, executable, options):
    output.mkdir(parents=True, exist_ok=False)
    root = output / "repository"
    root.mkdir()
    helper = load("group_executor", SOURCE / ".github/scripts/local_acceptance.py")
    helper.ROOT = repository
    profiles = load("group_profiles", SOURCE / ".github/local-acceptance/checks.py")
    environment = {"CARGO_TARGET_DIR": str(output / "target"), "SCODE_GROUP_PID_DIR": str(output / "pids")}
    if os.name == "nt":
        # Exact native canary equality must exclude Windows linker timestamps.
        encoded = os.environ.get("CARGO_ENCODED_RUSTFLAGS")
        if encoded is not None:
            environment["CARGO_ENCODED_RUSTFLAGS"] = encoded + "\x1f-C\x1flink-arg=/Brepro"
        else:
            environment["RUSTFLAGS"] = os.environ.get("RUSTFLAGS", "") + " -C link-arg=/Brepro"
    commands = []

    def command(*args, cwd=root / "rust", success=True):
        log = output / "commands" / f"{len(commands):03}.log"
        try:
            result = helper.run_command(args, cwd, log, environment)
        except RuntimeError:
            if success:
                raise
            result = json.loads(log.with_suffix(".command.json").read_text())
        else:
            if not success:
                raise AssertionError("Expected the real process to fail: " + str(args))
        commands.append(result)
        return log

    def git(*args):
        return command("git", *args, cwd=root).read_text(encoding="utf-8").strip()

    def write(path, text):
        destination = root / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(text, encoding="utf-8")

    def commit(message):
        git("add", ".")
        git("commit", "-m", message)
        return git("rev-parse", "HEAD")

    workspace = '[workspace]\nmembers=["crates/rusty-sudocode-cli","crates/helper"]\nresolver="2"\n'
    manifest = ('[package]\nname="group-product"\nversion="0.0.0"\nedition="2021"\n'
                '[dev-dependencies]\ngroup-helper={path="../helper"}\n')
    package = "rust/crates/rusty-sudocode-cli/"
    write("rust/Cargo.toml", workspace)
    write(package + "Cargo.toml", manifest)
    write(package + "src/main.rs", 'fn main() { println!("product-42"); }\n')
    write("rust/crates/helper/Cargo.toml", '[package]\nname="group-helper"\nversion="0.0.0"\nedition="2021"\n')
    write("rust/crates/helper/src/lib.rs", '''use std::sync::atomic::{AtomicUsize, Ordering};
static CASES: AtomicUsize = AtomicUsize::new(0);
pub fn record(name: &str) {
    assert_eq!(CASES.fetch_add(1, Ordering::SeqCst), 0, "cases must have distinct process state");
    let root = std::path::PathBuf::from(std::env::var_os("SCODE_GROUP_PID_DIR").unwrap());
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join(name), std::process::id().to_string()).unwrap();
}
''')
    common = '''pub fn native(name: &str) {
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_group-product")).output().unwrap();
    assert!(result.status.success());
    assert_eq!(String::from_utf8(result.stdout).unwrap().trim(), "product-42");
    group_helper::record(name);
}
'''
    write(package + "tests/common/mod.rs", common)
    write(package + "tests/first.rs", '''mod common;
#[test] fn native_one() { common::native("one"); }
#[test] #[ignore] fn ignored_guard() { panic!("ignored case executed"); }
''')
    write(package + "tests/second.rs", '''mod common;
#[test] fn native_two() { common::native("two"); }
#[cfg(windows)] #[test] fn platform_case() { common::native("windows"); }
#[cfg(unix)] #[test] fn platform_case() { common::native("unix"); }
''')
    write(package + "tests/kept_driver.rs", 'mod common;\n#[test] fn native_driver() { common::native("driver"); }\n')
    write(package + "tests/acceptance_reserved/main.rs",
          '#[test] fn directory_target() { group_helper::record("directory"); }\n')
    write("rust/crates/helper/tests/independent.rs", '#[test] fn independent_package() { group_helper::record("other-package"); }\n')
    for relative in [".github/scripts/local_acceptance.py", "spec/local_acceptance.json"]:
        write(relative, (SOURCE / relative).read_text(encoding="utf-8"))
    git("init", "--initial-branch=main")
    git("config", "user.name", "Grouped targets acceptance")
    git("config", "user.email", "groups@example.invalid")
    git("config", "core.autocrlf", "false")
    command("cargo", "generate-lockfile", "--offline")
    base = commit("Baseline individual integration targets")
    write(package + "src/main.rs", 'fn main() { println!("product-42"); } // Source change before grouping.\n')
    ungrouped = commit("Change native source without grouping")
    text = command(sys.executable, str(root / ".github/scripts/local_acceptance.py"), "plan", "--base", base,
                   "--policy-ref", base, "--host", "windows", "--architecture", "AMD64", cwd=root).read_text(encoding="utf-8")
    ungrouped_plan = json.loads(next(line.split("=", 1)[1] for line in text.splitlines() if line.startswith("ACCEPTANCE_PLAN=")))
    assert "native-workspace" in ungrouped_plan["local_checks"]
    assert "native-examples" not in ungrouped_plan["local_checks"], text
    base = ungrouped

    def metadata():
        return json.loads(command("cargo", "metadata", "--no-deps", "--locked", "--format-version=1").read_text(encoding="utf-8"))

    def inventory(arguments):
        text = command(str(executable), "nextest", "list", "--message-format", "json", "--workspace", "--locked",
                       *arguments).read_text(encoding="utf-8")
        decoder = json.JSONDecoder()
        for index, character in enumerate(text):
            if character != "{":
                continue
            try:
                value, _ = decoder.raw_decode(text[index:])
                if "rust-suites" in value:
                    return value
            except (ValueError, TypeError):
                continue
        raise AssertionError("Real nextest did not return its complete inventory")

    def normalize(value, plan):
        groups = {row["target"]: row["modules"] for row in plan.get("groups", [])}
        result = set()
        for suite in value["rust-suites"].values():
            binary = suite["binary-name"]
            for name, case in suite["testcases"].items():
                original, test = binary, name
                if binary in groups:
                    original, separator, test = name.partition("::")
                    assert separator and original in groups[binary], (binary, name)
                result.add((suite["package-name"], original, test, case["ignored"]))
        return sorted(result)

    def product():
        log = command("cargo", "build", "--locked", "--bin", "group-product", "--message-format=json")
        binary = output / "target/debug" / ("group-product.exe" if os.name == "nt" else "group-product")
        assert command(str(binary)).read_text().strip() == "product-42"
        return hashlib.sha256(binary.read_bytes()).hexdigest(), log

    original_binary, _ = product()
    original = inventory([])
    original_cases = normalize(original, {})
    group_name = "acceptance_fixture"
    wrapper = package + f"tests/acceptance_groups/{group_name}.rs"
    declaration = f'\n[[test]]\nname="{group_name}"\npath="tests/acceptance_groups/{group_name}.rs"\ntest=false\n'
    write(package + "Cargo.toml", manifest + declaration)
    canonical = profiles.test_group_source(["first", "second"])
    write(wrapper, canonical)
    grouped = commit("Add disabled opt-in grouping target")
    arguments, plan = profiles.grouped_workspace_test_targets(root, metadata())
    observed = inventory(arguments)
    assert normalize(observed, plan) == original_cases, "Grouping changed actual case identity or ignore state"
    digest, log = product()
    assert digest == original_binary, "Opt-in targets changed the normal native product"
    rows = [json.loads(line) for line in log.read_text().splitlines() if line.startswith("{")]
    assert all(row["fresh"] for row in rows if row.get("reason") == "compiler-artifact"), "Unchanged product rebuilt"
    control = command("cargo", "test", "--locked", "-p", "group-product", "--test", group_name,
                      "--", "--test-threads=1", success=False)
    assert "cases must have distinct process state" in control.read_text(encoding="utf-8")
    # Future individual targets remain selected even before authors assign them
    # to a group. Their original developer commands also keep working.
    write(package + "tests/future.rs", 'mod common;\n#[test] fn future_case() { common::native("future"); }\n')
    grown_arguments, grown_plan = profiles.grouped_workspace_test_targets(root, metadata())
    grown = normalize(inventory(grown_arguments), grown_plan)
    assert set(grown) - set(original_cases) == {("group-product", "future", "future_case", False)}
    assert set(original_cases) <= set(grown)
    command(str(executable), "nextest", "run", *options, "--workspace", "--locked", *grown_arguments)
    pid_files = sorted((output / "pids").iterdir())
    assert {path.name for path in pid_files} == {"one", "two", "driver", "directory", "other-package", "future",
                                              "windows" if os.name == "nt" else "unix"}
    command("cargo", "test", "--locked", "-p", "group-product", "--test", "kept_driver", "--", "--exact", "native_driver")
    command("cargo", "test", "--locked", "-p", "group-product", "--test", "first", "--", "--exact", "native_one")
    refusals = []
    for label, contents, changed_manifest in [
        ("duplicate-member", profiles.test_group_source(["first", "first"]), manifest + declaration),
        ("missing-member", profiles.test_group_source(["missing"]), manifest + declaration),
        ("additional-rust", canonical + 'fn hidden() {}\n', manifest + declaration),
        ("redirected-member", canonical.replace('../first.rs', '../other/first.rs'), manifest + declaration),
        ("unregistered-wrapper", canonical, manifest),
        ("enabled-alias", canonical, manifest + declaration.replace('test=false', 'test=true')),
        ("feature-hidden-group", canonical, manifest + '\n[features]\nhidden=[]\n' + declaration
         + 'required-features=["hidden"]\n'),
    ]:
        write(wrapper, contents)
        write(package + "Cargo.toml", changed_manifest)
        try:
            profiles.grouped_workspace_test_targets(root, metadata())
        except RuntimeError as error:
            refusals.append({"case": label, "error": str(error)})
        else:
            raise AssertionError(f"Invalid grouping accepted: {label}")
    write(wrapper, canonical)
    write(package + "Cargo.toml", manifest + declaration)
    for name in ["kept_driver", "acceptance_reserved"]:
        (root / wrapper).unlink()
        shadow = package + f"tests/acceptance_groups/{name}.rs"
        write(shadow, canonical)
        write(package + "Cargo.toml", manifest + declaration.replace(group_name, name))
        try:
            profiles.grouped_workspace_test_targets(root, metadata())
        except RuntimeError as error:
            assert "shadows an individual Cargo target" in str(error), str(error)
            refusals.append({"case": "shadow-" + name, "error": str(error)})
        else:
            raise AssertionError(f"Wrapper concealed an original Cargo target: {name}")
        (root / shadow).unlink()
        write(wrapper, canonical)
        write(package + "Cargo.toml", manifest + declaration)

    scope_cases = []
    def scope(label, expected):
        head = commit(label)
        text = command(sys.executable, str(root / ".github/scripts/local_acceptance.py"), "plan", "--base", base,
                       "--policy-ref", base, "--host", "windows", "--architecture", "AMD64", cwd=root).read_text(encoding="utf-8")
        planned = json.loads(next(line.split("=", 1)[1] for line in text.splitlines() if line.startswith("ACCEPTANCE_PLAN=")))
        checks = set(planned["local_checks"])
        costly = {"native-release", "native-services", "native-cohost"}
        assert (checks & costly == set()) if expected else (costly - {"native-cohost"}) <= checks, (label, text)
        # Manifests and test-group wrappers are not browser terminal inputs.
        assert "browser" not in checks, (label, text)
        assert "native-workspace" in checks
        if expected:
            assert "native-examples" in checks, text
        scope_cases.append({"case": label, "head": head, "opt_in_only": expected, "checks": sorted(checks)})

    scope("New disabled grouping target keeps native cases but skips unchanged products and benchmarks", True)
    for label, replacement, workspace_text, remove in [
        ("Enabled target keeps ordinary scope", declaration.replace('test=false', 'test=true'), workspace, False),
        ("Unknown target fields keep ordinary scope", declaration + 'harness=false\n', workspace, False),
        ("Existing auto target name keeps ordinary scope", declaration.replace(group_name, 'kept_driver'), workspace, False),
        ("Existing directory target name keeps ordinary scope", declaration.replace(group_name, 'acceptance_reserved'), workspace, False),
        ("Missing wrapper keeps ordinary scope", declaration, workspace, True),
        ("Changed workspace keeps ordinary scope", declaration, workspace.replace('resolver="2"', 'resolver="3"'), False),
        ("Malformed manifest keeps ordinary scope", '\n[[test]\n', workspace, False),
    ]:
        write("rust/Cargo.toml", workspace_text)
        write(package + "Cargo.toml", manifest + replacement)
        for reserved in ['kept_driver', 'acceptance_reserved']:
            if reserved in replacement:
                write(package + f'tests/acceptance_groups/{reserved}.rs', canonical)
        if remove:
            (root / wrapper).unlink()
        else:
            write(wrapper, canonical)
        scope(label, False)
    result = {"status": "PASS", "original_cases": original_cases, "grown_cases": grown,
              "exact_case_and_ignore_state_preserved": True, "nextest_process_state_isolated": True,
              "same_process_control_failed_isolation_assertion": True,
              "normal_product_sha256": original_binary, "normal_product_cargo_fresh": True,
              "original_named_driver_and_developer_target_passed": True,
              "invalid_group_refusals": refusals, "scope_cases": scope_cases, "ungrouped_plan": ungrouped_plan,
              "commands": commands,
              "scope": "Real Git commits/public planner, Cargo metadata/builds, original and grouped nextest inventories, actual per-case native child processes and independent process-state assertions. Tiny infrastructure fixtures, no product PTY/provider coverage credit."}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    print(json.dumps({"status": "PASS", "case_count": len(grown), "invalid_groups_refused": len(refusals),
                      "scope_cases": len(scope_cases), "evidence": str(output)}), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--repository", type=Path, default=SOURCE)
    parser.add_argument("--executable", required=True, type=Path)
    parser.add_argument("--options", required=True)
    args = parser.parse_args()
    run(args.output.resolve(), args.repository.resolve(), args.executable.resolve(), json.loads(args.options))
