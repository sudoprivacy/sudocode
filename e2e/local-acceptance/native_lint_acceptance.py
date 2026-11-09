"""Require real Cargo warnings to stop native acceptance before long work."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import threading


SOURCE = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def nextest_preflight(output, profiles):
    """Refuse real proxy failures and corrupt archives before any Cargo child."""
    output.mkdir(parents=True)
    seed = output / "seed"
    seed.mkdir()
    binary = profiles.nextest_binary(seed)
    archive = next(path for path in binary.parent.glob("cargo-nextest-*")
                   if path.name.endswith((".zip", ".tar.gz")))
    homes = tempfile.TemporaryDirectory(prefix="scode-tool-homes-")
    requests = []

    class UnavailableProxy(BaseHTTPRequestHandler):
        def do_CONNECT(self):
            requests.append(self.path)
            self.send_error(504, "Fixture gateway unavailable")

        def log_message(self, _format, *_args):
            pass

    proxy = ThreadingHTTPServer(("127.0.0.1", 0), UnavailableProxy)
    thread = threading.Thread(target=proxy.serve_forever, daemon=True)
    thread.start()
    driver = output / "actual-native-dispatcher.py"
    driver.write_text('''import importlib.util, json, subprocess, sys
from pathlib import Path
def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module
profiles = load("actual_profiles", sys.argv[1])
helper = load("actual_commands", sys.argv[2])
root, output = map(Path, sys.argv[3:5])
helper.ROOT = root
commands = []
def execute(command, cwd, log, env=None):
    commands.append(list(command))
    return helper.run_command(command, cwd, log, env)
result = {}
try:
    if sys.argv[5] == "native-guard":
        directory = output / "guard"
        policy = output / "guard-source"
        source = Path(profiles.__file__).resolve().parents[2]
        for relative in [".github/local-acceptance/checks.py", ".github/local-acceptance/native_guard.py",
                         ".github/scripts/local_acceptance.py"]:
            destination = policy / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_bytes((source / relative).read_bytes())
        process = subprocess.run([sys.executable, str(policy / ".github/local-acceptance/native_guard.py"),
            "--repository", str(root), "--output", str(directory), "--executable", sys.argv[6],
            "--source-receipt", sys.argv[7], "--options", "[]"], capture_output=True, text=True)
        assert process.returncode != 0
        assert not list(directory.glob("*.command.json")), "The full guard started commands before its tool refusal"
        raise RuntimeError(process.stdout + process.stderr)
    elif sys.argv[5] == "cached-valid":
        executable = profiles.nextest_binary(output)
        version = subprocess.check_output([str(executable), "nextest", "--version"], text=True)
        result.update(status="PASS", version=version.strip())
    else:
        profiles.run(sys.argv[5], root, output, "HEAD", execute)
        result["status"] = "UNEXPECTED_NATIVE_PASS"
except Exception as error:
    result.update(status="REFUSED", error=str(error))
finally:
    result["commands"] = commands
    (output / "child-result.json").write_text(json.dumps(result, indent=2))
''', encoding="utf-8")
    cases = []
    try:
        selections = [(name, state) for name in ["native-workspace", "native-resize", "native-services"]
                      for state in ["missing", "corrupt"]] + [("cached-valid", "valid")]
        if platform.system() in {"Windows", "Darwin"}:
            selections += [("native-cohost", "missing"), ("native-guard", "missing")]
        for name, state in selections:
            directory = output / (name + "-" + state)
            home = Path(homes.name) / (name + "-" + state)
            root, evidence = directory / "source", directory / "evidence"
            for path in [home, root / "rust", evidence]:
                path.mkdir(parents=True)
            if state != "missing":
                cached = home / archive.relative_to(Path.home())
                cached.parent.mkdir(parents=True)
                shutil.copyfile(archive, cached)
                if state == "corrupt":
                    contents = cached.read_bytes()
                    cached.write_bytes(bytes([contents[0] ^ 1]) + contents[1:])
            address = f"http://127.0.0.1:{proxy.server_port}"
            environment = {**os.environ, "HOME": str(home), "USERPROFILE": str(home),
                           "HTTPS_PROXY": address, "https_proxy": address, "NO_PROXY": "", "no_proxy": ""}
            before = len(requests)
            process = subprocess.run([sys.executable, str(driver), str(Path(profiles.__file__).resolve()),
                                      str(SOURCE / ".github/scripts/local_acceptance.py"), str(root),
                                      str(evidence), name, str(binary), str(seed / "nextest-source.json")],
                                     env=environment, capture_output=True, timeout=30)
            (directory / "stdout.log").write_bytes(process.stdout)
            (directory / "stderr.log").write_bytes(process.stderr)
            assert process.returncode == 0, process.stderr.decode(errors="replace")
            result = json.loads((evidence / "child-result.json").read_text())
            assert not result["commands"], result
            contacted = requests[before:]
            if state == "valid":
                assert result["status"] == "PASS" and result["version"].startswith("cargo-nextest 0.9.150 "), result
                assert not contacted, contacted
            else:
                assert result["status"] == "REFUSED", result
                assert ("504" if state == "missing" else "checksum mismatch") in result["error"], result
                if state == "missing":
                    assert contacted == ["github.com:443"], contacted
                else:
                    assert not contacted, contacted
            cases.append({"profile": name, "cache": state, "actual_proxy_requests": contacted,
                          "result": result, "dispatcher_commands_started": 0})
    finally:
        proxy.shutdown()
        proxy.server_close()
        thread.join(timeout=5)
        homes.cleanup()
    print("PASS: real nextest proxy504 and corrupted archives refuse before Cargo on all three profiles; "
          "the checksum-verified cached executable runs offline; Windows/macOS protoc proxy504 refuses "
          "before cohost Cargo or full native guard commands", flush=True)
    return cases


def prepared_lint(output, root, base, helper, profiles, environment):
    """Exercise real lint reuse and refusal without signing a partial workspace."""
    output.mkdir(parents=True)
    observed = []

    def execute(command, cwd, log, env=None):
        observed.append(list(command))
        return helper.run_command(command, cwd, log, env)

    preparation = profiles.prepare_native_workspace_lint(root, output, base, execute)
    directory = output / "native-workspace"
    assert not (output / "envelope.json").exists()
    originals = {path.name: path.read_bytes() for path in directory.iterdir() if path.is_file()}

    def refused(label, *, path=directory, selected_base=base):
        try:
            profiles.consume_workspace_lint(preparation, root, path, selected_base, [])
        except RuntimeError as error:
            print(f"EXPECTED_PREPARATION_REFUSAL {label}: {error}", flush=True)
        else:
            raise AssertionError(f"Accepted invalid lint preparation: {label}")

    log = directory / "000.log"
    log.write_bytes(log.read_bytes() + b"tampered\n")
    refused("modified original log")
    log.write_bytes(originals[log.name])
    refused("another output", path=output / "elsewhere")
    refused("another base", selected_base="HEAD")
    os.environ["CARGO_TARGET_DIR"] = str(output / "different-target")
    refused("another Cargo target")
    os.environ["CARGO_TARGET_DIR"] = environment["CARGO_TARGET_DIR"]
    dirty = root / "dirty.txt"
    dirty.write_text("not accepted source\n", encoding="utf-8")
    refused("dirty source")
    dirty.unlink()
    profiles.run.execution_refs = ("different-policy", "different-head")
    refused("another protected executor")
    del profiles.run.execution_refs
    subprocess.run(["git", "commit", "--allow-empty", "-m", "Different clean source"],
                   cwd=root, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    refused("another clean HEAD")
    subprocess.run(["git", "checkout", "--detach", preparation["identity"]["head"]],
                   cwd=root, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    profiles.run.prepared_workspace = preparation
    # The original full-workspace tail executes a real child which fails at
    # the fixture's metadata sentinel. Its failure must remain a failure.
    try:
        profiles.run("native-workspace", root, directory, base, execute)
    except RuntimeError:
        assert not hasattr(profiles.run, "prepared_workspace"), "Lint preparation was not consumed once"
        assert len([args for args in observed if args[1:2] == ["clippy"]]) == 1, observed
        assert observed[-1][1:2] == ["e2e/build-metadata/run.py"], observed
        assert json.loads((directory / f"{len(preparation['commands']):03d}.command.json").read_text())["exit_code"] == 9
    else:
        raise AssertionError("A later failing real workspace child passed")
    assert all((directory / name).read_bytes() == data for name, data in originals.items())
    return {"status": "PASS", "lint_commands": preparation["commands"],
            "original_log_sha256": hashlib.sha256(log.read_bytes()).hexdigest(),
            "lint_invocations": 1, "later_failure_preserved": True,
            "invalid_preparation_refused": 7,
            "scope": "Real Git heads, Cargo/Clippy and remaining native child; not completed product acceptance."}


def grouped_lint(output, helper, profiles):
    """Compare real compiler diagnostics through the production dispatcher."""
    root = output / "source"
    rust = root / "rust"
    files = {
        "Cargo.toml": '[package]\nname="lint-group-probe"\nversion="0.0.0"\nedition="2021"\n'
                      '[workspace]\nresolver="2"\n'
                      '[[test]]\nname="acceptance_group"\npath="tests/acceptance_groups/acceptance_group.rs"\ntest=false\n'
                      '[[test]]\nname="disabled_driver"\npath="tests/disabled_driver.rs"\ntest=false\n'
                      '[[bench]]\nname="probe_bench"\nharness=false\n',
        "src/lib.rs": 'pub fn value() -> u8 { 42 }\n',
        "src/main.rs": 'fn main() {}\n',
        "tests/first.rs": '#[path="support/ordinary.rs"]\nmod common;\n#[test]\nfn first() { let _ = common::value(); }\n',
        "tests/second.rs": '#[test]\nfn second() {}\n',
        "tests/support/ordinary.rs": 'pub fn value() -> u8 { 42 }\n',
        "tests/disabled_driver.rs": '#[test]\nfn disabled() {}\n',
        "tests/acceptance_groups/acceptance_group.rs": profiles.test_group_source(["first", "second"]),
        "examples/probe_example.rs": 'fn main() {}\n',
        "benches/probe_bench.rs": 'fn main() {}\n',
    }
    for name, contents in files.items():
        path = rust / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents, encoding="utf-8")
    env = {"CARGO_TARGET_DIR": str(output / "target"), "CARGO_BUILD_JOBS": "1"}
    commands = []

    def command(args):
        log = output / "commands" / f"{len(commands):03d}.log"
        try:
            record = helper.run_command(args, rust, log, env)
        except RuntimeError:
            record = json.loads(log.with_suffix(".command.json").read_text())
        commands.append(record)
        return record, log.read_text(encoding="utf-8")

    record, _ = command(["cargo", "generate-lockfile", "--offline"])
    assert record["exit_code"] == 0
    record, text = command(["cargo", "metadata", "--no-deps", "--locked", "--format-version=1"])
    assert record["exit_code"] == 0
    metadata = json.loads(text)
    targets, selection = profiles.grouped_workspace_test_targets(root, metadata)
    for args in [["git", "init", "--initial-branch=main"], ["git", "add", "."],
                 ["git", "-c", "user.name=Lint fixture", "-c", "user.email=lint@example.invalid",
                  "commit", "-m", "Grouped lint fixture"]]:
        subprocess.run(args, cwd=root, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    base = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root).decode().strip()
    # Obtain the actual lint argv from the production full-workspace branch.
    # Cargo and Clippy below execute normally; unrelated later product work is
    # deliberately not part of this private compiler fixture.
    observed = []
    class LintSelected(Exception):
        pass

    def observe(args, cwd, log, extra=None):
        observed.append(list(args))
        if list(args)[1:2] == ["clippy"]:
            raise LintSelected()
        log.parent.mkdir(parents=True, exist_ok=True)
        log.write_text(json.dumps(metadata) if list(args)[1:2] == ["metadata"] else "", encoding="utf-8")
        return {"exit_code": 0}
    try:
        profiles.run("native-workspace", root, output / "dispatch", base, observe)
    except LintSelected:
        pass
    lint = [row for row in observed if row[1:2] == ["clippy"]]
    assert len(lint) == 1, observed
    # The fixture has no mailbox package; remove only that feature selector.
    grouped = lint[0].copy()
    feature = grouped.index("--features")
    assert grouped[feature + 1] == "engine-acp/mailbox"
    del grouped[feature:feature + 2]
    original = ["cargo", "clippy", "--workspace", "--all-targets", "--locked", "--", "-D", "warnings"]
    inventory = {}
    for mode, argv in {"original": original, "grouped": grouped}.items():
        record, text = command([*argv[:-3], "--message-format=json", *argv[-3:]])
        assert record["exit_code"] == 0, mode
        rows = [json.loads(line) for line in text.splitlines() if line.startswith("{")]
        inventory[mode] = sorted({row["target"]["name"] for row in rows
                                  if row.get("reason") == "compiler-artifact" and row["target"]["kind"] == ["test"]})
    assert inventory == {"original": ["first", "second"], "grouped": ["acceptance_group"]}, inventory
    injection = '\n#[allow(dead_code)]\nfn diagnostic_marker() { let _ = 1 == 1; }\n'
    cases = {
        "ordinary-library": ("src/lib.rs", injection),
        "test-library": ("src/lib.rs", '\n#[cfg(test)]' + injection),
        "test-binary": ("src/main.rs", '\n#[cfg(test)]' + injection),
        "wrapped-module": ("tests/first.rs", injection),
        "ignored-test": ("tests/second.rs", '\n#[test]\n#[ignore]\nfn ignored_lint() { let diagnostic_unused = 1; }\n'),
        "shared-helper": ("tests/support/ordinary.rs", injection),
        "example": ("examples/probe_example.rs", injection),
        "bench": ("benches/probe_bench.rs", injection),
        "disabled-driver": ("tests/disabled_driver.rs", injection),
    }
    diagnostics = {}
    try:
        for case, (relative, addition) in cases.items():
            path = rust / relative
            path.write_text(files[relative] + addition, encoding="utf-8")
            diagnostics[case] = {}
            for mode, argv in {"original": original, "grouped": grouped}.items():
                record, text = command([*argv[:-3], "--message-format=json", *argv[-3:]])
                rows = [json.loads(line) for line in text.splitlines() if line.startswith("{")]
                code = "unused_variables" if case == "ignored-test" else "clippy::eq_op"
                errors = [row for row in rows if row.get("reason") == "compiler-message"
                          and (row["message"].get("code") or {}).get("code") == code]
                skipped = case == "disabled-driver"
                assert (record["exit_code"] == 0) == skipped, (case, mode, record)
                assert bool(errors) != skipped, (case, mode, text)
                diagnostics[case][mode] = {"exit_code": record["exit_code"], "diagnostic_code": code, "diagnostics": len(errors)}
            path.write_text(files[relative], encoding="utf-8")
    finally:
        for name, contents in files.items():
            (rust / name).write_text(contents, encoding="utf-8")
    return {"status": "PASS", "selection": selection, "integration_inventory": inventory,
            "diagnostics": diagnostics, "commands": commands,
            "scope": "Actual production lint argv, real Cargo and Clippy. No product execution or timing claim."}


def run(output, executor=None):
    output.mkdir(parents=True, exist_ok=False)
    parent = load("lint_fixture_environment", SOURCE / "e2e/local-acceptance/run.py")
    os.environ.clear()
    os.environ.update(parent.FIXTURE_ENV)
    helper = load("lint_fixture_commands", SOURCE / ".github/scripts/local_acceptance.py")
    helper.ROOT = SOURCE
    profiles = load("lint_fixture_profiles", executor or SOURCE / ".github/local-acceptance/checks.py")
    tool_preflight = nextest_preflight(output / "nextest-preflight", profiles)
    prepared = None
    for kind in ["workspace", "selected"]:
        root = output / kind / "source"
        rust = root / "rust"
        cli = rust / "crates/rusty-sudocode-cli"
        acp = rust / "crates/engine-acp"
        (cli / "src").mkdir(parents=True)
        (cli / "tests").mkdir()
        (acp / "src").mkdir(parents=True)
        (rust / "Cargo.toml").write_text(
            '[workspace]\nresolver="2"\nmembers=["crates/rusty-sudocode-cli","crates/engine-acp"]\n', encoding="utf-8")
        (cli / "Cargo.toml").write_text(
            '[package]\nname="rusty-sudocode-cli"\nversion="0.0.0"\nedition="2021"\n'
            '[dependencies]\nengine-acp={path="../engine-acp"}\n', encoding="utf-8")
        (acp / "Cargo.toml").write_text(
            '[package]\nname="engine-acp"\nversion="0.0.0"\nedition="2021"\n[features]\nmailbox=[]\n', encoding="utf-8")
        (acp / "src/lib.rs").write_text('pub fn value() -> u8 {\n    42\n}\n', encoding="utf-8")
        main = cli / "src/main.rs"
        test = cli / "tests/selected.rs"
        main.write_text('fn main() {}\n', encoding="utf-8")
        test.write_text('#[test]\nfn selected() {}\n', encoding="utf-8")
        scripts = root / "scripts"
        scripts.mkdir()
        for name in ["generate_cache_metrics.py", "check_cache_usage_fixtures.py"]:
            (scripts / name).write_text('print("Fixture metadata check")\n', encoding="utf-8")
        sentinel = output / kind / "late-work-started"
        later = root / "e2e/build-metadata"
        later.mkdir(parents=True)
        (later / "run.py").write_text(
            f'from pathlib import Path\nPath({str(sentinel)!r}).touch()\nraise SystemExit(9)\n', encoding="utf-8")
        def git(*args):
            return subprocess.check_output(["git", *args], cwd=root).decode().strip()
        git("init", "--initial-branch=main")
        git("config", "user.name", "Native lint acceptance")
        git("config", "user.email", "lint@example.invalid")
        helper.run_command(["cargo", "generate-lockfile", "--offline"], rust, output / kind / "lock.log")
        git("add", ".")
        git("commit", "-m", "Native lint fixture")
        base = git("rev-parse", "HEAD")
        changed = main if kind == "workspace" else test
        changed.write_text('fn unused() {}\n\n' + changed.read_text(encoding="utf-8"), encoding="utf-8")
        git("add", ".")
        git("commit", "-m", "Introduce a real compiler warning")
        os.environ["CARGO_TARGET_DIR"] = str(output / kind / "target")
        observed = []
        def execute(command, cwd, log, env=None):
            observed.append(list(command))
            return helper.run_command(command, cwd, log, env)
        print(f"Expected negative control: {kind} compiler warning must stop before long work.", flush=True)
        try:
            profiles.run("native-workspace", root, output / kind / "acceptance", base, execute)
        except RuntimeError:
            lint = [command for command in observed if len(command) > 1 and command[1] == "clippy"]
            assert len(lint) == 1 and lint[0][-3:] == ["--", "-D", "warnings"], observed
            logs = '\n'.join(path.read_text(encoding="utf-8", errors="replace")
                             for path in (output / kind / "acceptance").glob("*.log"))
            assert "unused" in logs and "dead_code" in logs, logs[-2000:]
            assert not sentinel.exists(), "Long validation began before the warning was refused"
            assert not any("nextest" in str(command[0]) for command in observed), observed
        else:
            raise AssertionError("A native compiler warning passed the strict lint gate")
        print(f"Expected negative control: {kind} preparation must refuse the same real warning.", flush=True)
        try:
            profiles.prepare_native_workspace_lint(root, output / kind / "prepared-warning", base, execute)
        except RuntimeError:
            assert not sentinel.exists(), "Long work began after failed preparation"
        else:
            raise AssertionError("A real compiler warning passed early lint preparation")
        if kind == "workspace":
            changed.write_text('fn main() {}\n', encoding="utf-8")
            git("add", ".")
            git("commit", "-m", "Repair the actual compiler warning")
            prepared = prepared_lint(output / kind / "prepared-success", root, base, helper, profiles,
                                     {"CARGO_TARGET_DIR": os.environ["CARGO_TARGET_DIR"]})
    grouped = grouped_lint(output / "grouped", helper, profiles)
    result = {"status": "PASS", "workspace_warning_refused": True, "selected_test_warning_refused": True,
              "nextest_preflight": tool_preflight,
              "grouped_lint": grouped,
              "prepared_lint": prepared,
              "long_validation_and_nextest_unrun": True,
              "scope": "Actual native dispatcher, Git, Cargo and compiler warnings. Tiny enforcement fixture; no product journey coverage."}
    (output / "result.json").write_text(json.dumps(result, indent=2) + '\n', encoding="utf-8")
    print(json.dumps(result), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--executor", type=Path)
    args = parser.parse_args()
    run(args.output.resolve(), args.executor)
