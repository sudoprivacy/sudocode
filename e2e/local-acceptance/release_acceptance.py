#!/usr/bin/env python3
"""Run the actual release executor with Cargo and distinct native binaries.

Tiny process fixtures prove artifact identity, workspace-driver reuse and failure
propagation. They do not establish product PTY or live-provider coverage.
"""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess


SOURCE = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def color_environment(output):
    """Exercise actual Cargo/SDK setup with inherited and explicit color settings."""
    output.mkdir()
    helper = load("color_environment_executor", SOURCE / ".github/scripts/local_acceptance.py")
    project = output / "project"
    (project / "src").mkdir(parents=True)
    (project / "Cargo.toml").write_text(
        '[package]\nname="color-environment-fixture"\nversion="0.0.1"\nedition="2021"\n', encoding="utf-8")
    (project / "src/main.rs").write_text('''fn main() {
    let color = match std::env::var("NO_COLOR") {
        Ok(value) if value.is_empty() => "<empty>".to_owned(),
        Ok(value) => value,
        Err(_) => "<unset>".to_owned(),
    };
    println!("NO_COLOR={color}");
    println!("SDK_READY={}", std::env::var("LIB").is_ok_and(|value| !value.is_empty()));
}
''', encoding="utf-8")
    previous_color, previous_lib = os.environ.get("NO_COLOR"), os.environ.get("LIB")
    records = []
    try:
        os.environ["NO_COLOR"] = "1"
        if os.name == "nt":
            os.environ.pop("LIB", None)
        for stage, explicit, expected in [("inherited", {}, "<unset>"),
                ("explicit", {"NO_COLOR": "1"}, "1"), ("empty", {"NO_COLOR": ""}, "<empty>")]:
            log = output / (stage + ".log")
            records.append(helper.run_command(["cargo", "run", "--offline", "--quiet"], project, log,
                {"CARGO_TARGET_DIR": str(output / "target"), **explicit}))
            content = log.read_text(encoding="utf-8")
            assert f"NO_COLOR={expected}\n" in content, (
                "Windows SDK setup restored the launcher's NO_COLOR preference; clear inherited "
                f"NO_COLOR after SDK setup and retain explicit fixture values. {stage}: {content}")
            if os.name == "nt":
                assert "SDK_READY=true\n" in content, "The actual compiler child lost its SDK environment"
                assert helper.MSVC_ENV.get("NO_COLOR") == "1", "SDK setup did not capture the controlled parent"
            assert os.environ.get("NO_COLOR") == "1", "The executor changed its parent environment"
    finally:
        for name, previous in [("NO_COLOR", previous_color), ("LIB", previous_lib)]:
            if previous is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = previous
    return records


def service_graph(profiles, root, output, target, execute, driver):
    """Exercise Cargo freshness and native dispatch; RPC coverage stays in the real journeys."""
    scripts = root / "e2e/shared-services"
    scripts.mkdir(parents=True)
    process = '''import argparse, json, subprocess
from pathlib import Path
p = argparse.ArgumentParser()
p.add_argument('--scode', type=Path, required=True)
p.add_argument('--artifacts', type=Path, required=True)
p.add_argument('--grpcurl')
a = p.parse_args()
actual = subprocess.check_output([str(a.scode), 'service-graph-probe'], text=True).strip()
assert actual == 'debug=true,flavor=dev,mailbox=true', actual
a.artifacts.mkdir(parents=True)
(a.artifacts / 'native-child.json').write_text(json.dumps({'actual': actual}))
'''
    for name in ["run.py", "existing_box.py"]:
        (scripts / name).write_text(process, encoding="utf-8")
    bash = root / "e2e/nexus-a2a"
    bash.mkdir(parents=True)
    # This fixture tests native artifact selection, not gRPC. The real profile
    # separately runs both unchanged private-Node journeys before publication.
    (bash / "lib.sh").write_text('grpcurl_bin() { printf fixture-unused-rpc; }\n'
                                'native_path() { printf "%s" "$1"; }\n', encoding="utf-8")
    binary = target / "debug" / ("scode.exe" if os.name == "nt" else "scode")
    before = [(p.stat().st_mtime_ns, hashlib.sha256(p.read_bytes()).hexdigest()) for p in [binary, driver]]
    records = profiles.run("native-services", root, output / "services", "fixture-base", execute)
    assert len(records) == 3, "Cargo verification and both service processes remain required"
    assert "Compiling " not in (output / "services/000.log").read_text(encoding="utf-8")
    assert before == [(p.stat().st_mtime_ns, hashlib.sha256(p.read_bytes()).hexdigest()) for p in [binary, driver]]
    for name in ["services", "existing-box"]:
        assert json.loads((output / "services" / name / "native-child.json").read_text())["actual"] == (
            "debug=true,flavor=dev,mailbox=true")
    previous = os.environ["CARGO_TARGET_DIR"]
    try:
        os.environ["CARGO_TARGET_DIR"] = str(output / "standalone-target")
        standalone = profiles.run("native-services", root, output / "standalone", "fixture-base", execute)
    finally:
        os.environ["CARGO_TARGET_DIR"] = previous
    assert len(standalone) == 3
    assert "Compiling " in (output / "standalone/000.log").read_text(encoding="utf-8")
    source = root / "rust/src/main.rs"
    original = source.read_bytes()
    try:
        source.write_bytes(original + b'\ncompile_error!("changed candidate must rebuild before services");\n')
        try:
            profiles.run("native-services", root, output / "stale", "fixture-base", execute)
            raise AssertionError("A stale existing binary hid invalid current source")
        except RuntimeError:
            failure = json.loads((output / "stale/000.command.json").read_text(encoding="utf-8"))
            assert failure["exit_code"] == 101, failure
            assert "changed candidate must rebuild before services" in (output / "stale/000.log").read_text(encoding="utf-8")
            assert not (output / "stale/001.log").exists(), "Services ran before current-source compilation succeeded"
    finally:
        source.write_bytes(original)
    return {"warm_commands": records, "standalone_commands": standalone,
            "same_binary_and_driver": before, "stale_source_exit": failure["exit_code"]}


def resize_graph(profiles, root, output, target, execute):
    """Run real Git/Cargo/native children with controlled installer/npm boundaries."""
    terminal = root / "e2e/terminal-resize"
    terminal.mkdir(parents=True)
    installer = '''import os
from pathlib import Path
p = Path(os.environ['SCODE_RESIZE_FIXTURE_MARKERS'])
p.mkdir(parents=True, exist_ok=True)
(p / Path(__file__).stem).write_text('controlled installer boundary')
'''
    for name in ["setup_windows.py", "check_windows_setup.py"]:
        (terminal / name).write_text(installer, encoding="utf-8")
    names = ["pty_chrome_resize", "pty_prose_preview", "pty_review_scrollback", "pty_codex_colors"]
    tests = root / "rust/crates/rusty-sudocode-cli/tests"
    tests.mkdir(parents=True)
    manifest = root / "rust/Cargo.toml"
    declarations = "".join(f'\n[[test]]\nname="{name}"\npath="crates/rusty-sudocode-cli/tests/{name}.rs"\n' for name in names)
    manifest.write_text(manifest.read_text(encoding="utf-8") + declarations, encoding="utf-8")
    for name in names:
        (tests / (name + ".rs")).write_text('''#[test]
#[ignore = "controlled real native child"]
fn selected_native_case() {
    assert!(engine_acp::MAILBOX, "resize must retain the workspace feature graph");
    let expected = std::env::var("SCODE_EXPECT_WORKSPACE_GRAPH").unwrap() == "1";
    assert_eq!(shared_support::workspace_graph(), expected, "package selection changed feature union");
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_scode"))
        .arg("service-graph-probe").output().unwrap();
    assert!(child.status.success());
    assert_eq!(String::from_utf8(child.stdout).unwrap().trim(), "debug=true,flavor=dev,mailbox=true");
}
#[test]
fn nonignored_case_must_not_run() { panic!("resize widened its ignored-only selection"); }
''', encoding="utf-8")
    sequence = 0

    def git(*args):
        nonlocal sequence
        sequence += 1
        log = output / f"resize-git-{sequence}.log"
        execute(["git", *args], root, log)
        return log.read_text(encoding="utf-8").strip()

    def commit():
        git("add", ".")
        git("-c", "user.name=Acceptance fixture", "-c", "user.email=acceptance@example.invalid",
            "-c", "commit.gpgsign=false", "commit", "-m", "Fixture input transition")
        return git("rev-parse", "HEAD")

    git("init", "--initial-branch=main")
    base = commit()
    (root / "rust/src/ordinary.rs").write_text("// Ordinary Rust input.\n", encoding="utf-8")
    (root / "README.md").write_text("Ordinary documentation.\n", encoding="utf-8")
    ordinary = commit()
    arguments = ["cargo", "test", "--no-run", "--locked", "--workspace",
                 "--features", "engine-acp/mailbox"]
    for name in names:
        arguments += ["--test", name]
    execute(arguments, root / "rust", output / "resize-workspace-warm.log")
    binary = target / "debug" / ("scode.exe" if os.name == "nt" else "scode")
    before = (binary.stat().st_mtime_ns, hashlib.sha256(binary.read_bytes()).hexdigest())
    observations = []

    def check(label, reference, required, isolated=False):
        directory = output / label
        requested = []

        def boundary(args, cwd, log, env=None):
            requested.append(list(map(str, args)))
            # Real npm and pinned-host integration remain in the product PTYs.
            # This infrastructure fixture controls only dependency acquisition.
            actual = [str(binary), "service-graph-probe"] if str(args[0]) == "npm" else args
            return execute(actual, cwd, log, {**(env or {}),
                "SCODE_RESIZE_FIXTURE_MARKERS": str(directory / "markers"),
                "SCODE_EXPECT_WORKSPACE_GRAPH": "0" if isolated else "1"})

        previous = {key: os.environ.get(key) for key in ["GIT_DIR", "GIT_WORK_TREE"]}
        os.environ.update(GIT_DIR=str(SOURCE / ".git"), GIT_WORK_TREE=str(SOURCE))
        try:
            records = profiles.run("native-resize", root, directory, reference, boundary)
        finally:
            for key, value in previous.items():
                if value is None:
                    os.environ.pop(key, None)
                else:
                    os.environ[key] = value
        scope = json.loads((directory / "installer-scope.json").read_text(encoding="utf-8"))
        assert scope["fault_injection_required"] == required, scope
        assert (directory / "markers/check_windows_setup").exists() == (required and os.name == "nt")
        if os.name == "nt":
            assert (directory / "markers/setup_windows").exists(), "Real setup selection was removed"
        commands = [row for row in requested if row[0] == "cargo"]
        expected = names[:3] if os.name == "nt" else names
        assert [row[row.index("--test") + 1] for row in commands] == expected
        for row in commands:
            assert ("--workspace" in row) != isolated, row
            assert row[row.index("--features") + 1] == "engine-acp/mailbox"
            assert row[-3:] == ["--", "--ignored", "--nocapture"]
        for index, row in enumerate(records):
            log = directory / f"{index:03d}.log"
            if "test" in row["command"]:
                text = log.read_text(encoding="utf-8")
                assert "1 passed; 0 failed; 0 ignored" in text and "1 filtered out" in text, text
                assert "Compiling " not in text, "Warm resize driver was rebuilt"
        assert before == (binary.stat().st_mtime_ns, hashlib.sha256(binary.read_bytes()).hexdigest())
        observations.append({"case": label, "scope": scope, "isolated_cli_tests": isolated, "commands": records})

    check("resize-rust-docs", base, False)
    extra = root / "unknown-installer-helper.py"
    extra.write_text("# Unknown installer helper.\n", encoding="utf-8")
    (root / "rust/src/ordinary.rs").write_text("// Mixed Rust and unknown helper inputs.\n", encoding="utf-8")
    unknown = commit()
    check("resize-unknown", ordinary, True)
    extra.rename(root / "rust/src/renamed.rs")
    renamed = commit()
    check("resize-renamed", unknown, True)
    check("resize-unresolved", "missing-acceptance-base", True)
    source = tests / "pty_chrome_resize.rs"
    source.write_text(source.read_text(encoding="utf-8") + "\n// Isolated CLI test input.\n", encoding="utf-8")
    commit()
    isolated = list(arguments)
    position = isolated.index("--workspace")
    isolated[position:position + 1] = ["-p", "rusty-sudocode-cli"]
    execute(isolated, root / "rust", output / "resize-isolated-warm.log")
    before = (binary.stat().st_mtime_ns, hashlib.sha256(binary.read_bytes()).hexdigest())
    check("resize-isolated-cli", renamed, False, isolated=True)
    return {"observations": observations, "same_warm_binary": before,
            "native_targets": names[:3] if os.name == "nt" else names,
            "scope": "Git/Cargo/process enforcement only; installer/npm acquisition is controlled, no product journey credit."}


def run(output):
    output.mkdir(parents=True, exist_ok=False)
    profiles = load("release_profiles", SOURCE / ".github/local-acceptance/checks.py")
    helper = load("release_executor", SOURCE / ".github/scripts/local_acceptance.py")
    root, target = output / "project", output / "target"
    rust = root / "rust"
    files = {
        "rust/Cargo.toml": '''[package]
name="rusty-sudocode-cli"
version="0.0.1"
edition="2021"
[workspace]
members=["support", "engine-acp", "auxiliary"]
resolver="2"
[[bin]]
name="scode"
path="src/main.rs"
[dependencies]
shared-support={path="support",features=["normal"]}
engine-acp={path="engine-acp"}
[dev-dependencies]
shared-support={path="support",features=["dev"]}
''',
        "rust/src/main.rs": '''fn main() {
    if std::env::args().nth(1).as_deref() == Some("service-graph-probe") {
        println!("debug={},flavor={},mailbox={}", cfg!(debug_assertions),
            shared_support::flavor(), engine_acp::MAILBOX);
        return;
    }
    let structured = std::env::args().nth(1).as_deref() == Some("version");
    if std::env::var_os("SCODE_TRACE_STARTUP").is_some() {
        eprintln!("startup phase=console elapsed_us=1 cumulative_us=1");
        eprintln!("startup phase=clap elapsed_us=1 cumulative_us=2");
        if structured { eprintln!("startup phase=version_output elapsed_us=1 cumulative_us=3"); }
    }
    if structured {
        println!(r#"{{"debug":{},"flavor":"{}"}}"#, cfg!(debug_assertions), shared_support::flavor());
    } else {
        println!("debug={},flavor={}", cfg!(debug_assertions), shared_support::flavor());
    }
}
''',
        "rust/support/Cargo.toml": '[package]\nname="shared-support"\nversion="0.0.1"\nedition="2021"\n'
            '[features]\nnormal=[]\ndev=[]\nworkspace-graph=[]\n',
        "rust/support/src/lib.rs": 'pub fn flavor() -> &\'static str { if cfg!(feature="dev") { "dev" } else { "normal" } }\n'
            'pub fn workspace_graph() -> bool { cfg!(feature="workspace-graph") }\n',
        "rust/auxiliary/Cargo.toml": '[package]\nname="auxiliary"\nversion="0.0.1"\nedition="2021"\n'
            '[dependencies]\nshared-support={path="../support",features=["workspace-graph"]}\n',
        "rust/auxiliary/src/lib.rs": 'pub fn uses_workspace_graph() -> bool { shared_support::workspace_graph() }\n',
        "rust/engine-acp/Cargo.toml": '[package]\nname="engine-acp"\nversion="0.0.1"\nedition="2021"\n'
            '[features]\nmailbox=[]\n',
        "rust/engine-acp/src/lib.rs": 'pub const MAILBOX: bool = cfg!(feature="mailbox");\n',
        "rust/tests/pty_model_stream_close.rs": '''#[test]
fn raw_stream_capture_keeps_streaming_and_resume_responsive() {
    assert!(cfg!(debug_assertions), "Only the driver uses the test profile");
    assert!(engine_acp::MAILBOX, "Reuse the same native workspace feature graph");
    assert!(shared_support::workspace_graph(), "Preserve features enabled by other workspace members");
    assert_eq!(shared_support::flavor(), "dev");
    let binary = std::env::var_os("SCODE_TEST_BIN").expect("Explicit release artifact required");
    assert_ne!(std::path::Path::new(&binary), std::path::Path::new(env!("CARGO_BIN_EXE_scode")));
    let output = std::process::Command::new(binary).output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "debug=false,flavor=normal");
}
#[test]
fn unselected_case_must_not_run() { panic!("release profile widened its selection"); }
''',
    }
    for relative, contents in files.items():
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents, encoding="utf-8")
    startup = root / "e2e/startup"
    startup.mkdir(parents=True)
    for name in ("run.py", "reference.rs"):
        (startup / name).write_bytes((SOURCE / "e2e/startup" / name).read_bytes())
    environment = {"CARGO_TARGET_DIR": str(target), "SCODE_TEST_BACKEND": "mock"}
    commands = []

    def execute(args, cwd, log, env=None):
        record = helper.run_command(args, cwd, log, {**environment, **(env or {})})
        commands.append(record)
        return record

    def cargo(*args, name):
        return execute(["cargo", *args], rust, output / (name + ".log"))

    cargo("generate-lockfile", name="lockfile")
    cargo("test", "--workspace", "--locked", "--features", "engine-acp/mailbox", "--test", "pty_model_stream_close",
          "--no-run", "--message-format=json-render-diagnostics", name="workspace-driver")
    artifacts = [json.loads(line) for line in (output / "workspace-driver.log").read_text(encoding="utf-8").splitlines()
                 if line.startswith("{")]
    driver = Path(next(row["executable"] for row in artifacts if row.get("reason") == "compiler-artifact"
                       and row.get("target", {}).get("name") == "pty_model_stream_close"))
    before = (driver.stat().st_mtime_ns, hashlib.sha256(driver.read_bytes()).hexdigest())
    previous = os.environ.get("CARGO_TARGET_DIR")
    os.environ["CARGO_TARGET_DIR"] = str(target)
    try:
        records = profiles.run("native-release", root, output / "release", "fixture-base", execute)
    finally:
        if previous is None:
            os.environ.pop("CARGO_TARGET_DIR", None)
        else:
            os.environ["CARGO_TARGET_DIR"] = previous
    assert len(records) == 3, "Release build, exact native driver and startup budget must all execute"
    assert before == (driver.stat().st_mtime_ns, hashlib.sha256(driver.read_bytes()).hexdigest()), "Workspace driver rebuilt"
    driver_log = (output / "release/001.log").read_text(encoding="utf-8")
    assert "1 passed; 0 failed; 0 ignored" in driver_log and "1 filtered out" in driver_log, driver_log
    assert "Compiling " not in driver_log, "The exact workspace driver graph should already be warm"
    suffix = ".exe" if os.name == "nt" else ""
    binary = output / "release" / ("scode" + suffix)
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    assert digest == hashlib.sha256((target / "release" / ("scode" + suffix)).read_bytes()).hexdigest()
    assert subprocess.check_output([str(binary)]).decode().strip() == "debug=false,flavor=normal"
    startup_report = json.loads((output / "release/startup.json").read_text(encoding="utf-8"))
    assert startup_report["binary"] == str(binary.resolve()) and startup_report["binary_bytes"] == binary.stat().st_size
    assert all(row["samples"] == 40 and row["budget_ms"] == 100 for row in startup_report["commands"])
    print("Expected negative: substituting the debug binary must fail the exact release-artifact assertion.", flush=True)
    wrong_binary = output / ("wrong-scode" + suffix)
    shutil.copy2(target / "debug" / ("scode" + suffix), wrong_binary)
    try:
        execute([str(driver), "raw_stream_capture_keeps_streaming_and_resume_responsive", "--exact", "--nocapture"],
                rust, output / "wrong-artifact.log", {"SCODE_TEST_BIN": str(wrong_binary)})
        raise AssertionError("Wrong debug artifact unexpectedly accepted")
    except RuntimeError:
        negative = json.loads((output / "wrong-artifact.command.json").read_text(encoding="utf-8"))
        assert negative["exit_code"] == 101, negative
        assert "debug=true,flavor=dev" in (output / "wrong-artifact.log").read_text(encoding="utf-8")
    os.environ["CARGO_TARGET_DIR"] = str(target)
    try:
        services = service_graph(profiles, root, output, target, execute, driver)
        resize = resize_graph(profiles, root, output, target, execute)
    finally:
        if previous is None:
            os.environ.pop("CARGO_TARGET_DIR", None)
        else:
            os.environ["CARGO_TARGET_DIR"] = previous
    color_records = color_environment(output / "color-environment")
    (output / "result.json").write_text(json.dumps({
        "status": "PASS", "commands": commands, "release_sha256": digest,
        "color_environment_commands": color_records,
        "service_graph": services,
        "resize_graph": resize,
        "driver_sha256": before[1], "startup": startup_report,
        "scope": "Real Cargo/native release executor fixture; not product PTY or provider coverage."}, indent=2), encoding="utf-8")
    print(f"PASS: normal release child, reused driver, startup budget, wrong-artifact refusal, "
          f"service reuse/standalone/stale-source, resize input scope/warm ignored-only guards and color settings "
          f"after Cargo/SDK setup; {output}", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    run(args.output.resolve())
