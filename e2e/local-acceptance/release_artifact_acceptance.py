"""Exercise the release entry point with real Git/Cargo/native children, not product coverage."""
import argparse
import ast
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile

SOURCE = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def workflow_contract():
    """Refuse unfamiliar job/step layouts instead of guessing about publication order."""
    workflow = (SOURCE / ".github/workflows/release.yml").read_text(encoding="utf-8")
    jobs = re.split(r"(?m)^  ([a-zA-Z0-9_-]+):\n", workflow.split("jobs:\n", 1)[1])
    observed = []
    for name, body in zip(jobs[1::2], jobs[2::2]):
        if name not in {"build", "build-linux-x64"}:
            continue
        steps = re.split(r"(?m)^      - ", body)
        gates = [index for index, step in enumerate(steps) if "uses: ./.github/actions/release-artifact\n" in step]
        packages = [index for index, step in enumerate(steps) if step.startswith("name: Stage release payload\n")]
        assert len(gates) == len(packages) == 1 and gates[0] < packages[0], name
        assert not re.search(r"(?m)^        (?:if|continue-on-error):", steps[gates[0]]), name
        assert not re.search(r"(?m)^        (?:if|continue-on-error):", steps[packages[0]]), name
        observed.append(name)
    assert set(observed) == {"build", "build-linux-x64"}
    action = (SOURCE / ".github/actions/release-artifact/action.yml").read_text(encoding="utf-8")
    gate = action.split("    - name: Check the exact built executable before packaging\n", 1)[1].split("    - ", 1)[0]
    assert not re.search(r"(?m)^      (?:if|continue-on-error):", gate)
    body = gate.split("      run: |\n", 1)[1]
    tree = ast.parse("\n".join(line[8:] for line in body.splitlines() if line.strip()))
    assert len(tree.body) == 2 and isinstance(tree.body[0], ast.Import) and isinstance(tree.body[1], ast.Expr)
    call = tree.body[1].value
    assert isinstance(call, ast.Call) and ast.unparse(call.func) == "subprocess.run"
    assert len(call.keywords) == 1 and call.keywords[0].arg == "check" and call.keywords[0].value.value is True
    assert '.github/scripts/release_artifact_acceptance.py' in ast.unparse(call.args[0])
    assert "||" not in gate and "continue-on-error" not in gate
    assert "needs.build-linux-x64.result == 'success' && needs.build.result == 'success'" in workflow
    assert "pattern: scode-*" in workflow, "Evidence artifacts must not be packaged as release assets"
    ci = (SOURCE / ".github/workflows/rust-ci.yml").read_text(encoding="utf-8")
    fixture = ci.split("      - name: Exercise native artifact gates and publication refusal paths\n", 1)[1].split("      - ", 1)[0]
    assert "github.event_name == 'pull_request'" in fixture
    assert "steps.scope.outputs.release_artifact_required == 'true'" in fixture
    assert "python3 e2e/local-acceptance/release_artifact_acceptance.py --output" in fixture
    return observed


def windows_compiler_environment(output, execute):
    """Compile and run native code through both real shells and a spaced SDK path."""
    if os.name != "nt":
        return {"status": "not-applicable", "system": sys.platform}
    shells = [Path(os.environ["SystemRoot"]) / "System32/WindowsPowerShell/v1.0/powershell.exe",
              Path(shutil.which("pwsh.exe") or "missing-pwsh")]
    if not all(shell.is_file() for shell in shells):
        raise RuntimeError("Release compiler acceptance requires Windows PowerShell and PowerShell 7 on PATH")
    script = SOURCE / ".github/actions/release-artifact/msvc.ps1"
    baseline = output / "compiler-environment.txt"
    execute([str(shells[-1]), "-NoLogo", "-NoProfile", "-NonInteractive", "-File", str(script),
             "-EnvironmentFile", str(baseline)], SOURCE, output / "compiler-discovery.log")

    def settings(path):
        return {key.upper(): value for line in path.read_text(encoding="utf-8-sig").splitlines()
                for key, value in [line.split("=", 1)]}

    discovered = settings(baseline)
    compiler = Path(discovered["VSINSTALLDIR"]) / "Common7/Tools/VsDevCmd.bat"
    assert compiler.is_file()
    wrapper = output / "Visual Studio path with spaces" / "VsDevCmd.bat"
    wrapper.parent.mkdir()
    wrapper.write_text(f'@echo off\ncall "{compiler}" %*\n', encoding="utf-8")
    controls = []
    for shell in shells:
        case = output / ("compiler-" + shell.stem)
        case.mkdir()
        environment = case / "environment.txt"
        execute([str(shell), "-NoLogo", "-NoProfile", "-NonInteractive", "-File", str(script),
                 "-EnvironmentFile", str(environment)], SOURCE, case / "prepare.log",
                {"GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "localAcceptance.vsdevcmd",
                 "GIT_CONFIG_VALUE_0": str(wrapper)})
        exported = settings(environment)
        assert all(exported.get(key) for key in ["PATH", "LIB", "INCLUDE"])
        assert exported["VSCMD_ARG_TGT_ARCH"] == discovered["VSCMD_ARG_TGT_ARCH"]
        native_compiler = shutil.which("cl.exe", path=exported["PATH"])
        assert native_compiler
        source, binary = case / "compiler.c", case / "compiler.exe"
        source.write_text('#include <stdio.h>\nint main(void) { puts("release compiler accepted"); return 0; }\n')
        execute([native_compiler, "/nologo", "/W4", "/WX", str(source), "/Fe:" + str(binary),
                 "/Fo:" + str(case / "compiler.obj"), "/link", "/incremental:no"],
                case, case / "build.log", exported)
        execute([str(binary)], case, case / "run.log")
        assert (case / "run.log").read_text().strip() == "release compiler accepted"
        controls.append({"shell": str(shell), "target": exported["VSCMD_ARG_TGT_ARCH"],
                         "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest()})
    return {"status": "PASS", "controls": controls}


def run(output):
    output.mkdir(parents=True, exist_ok=False)
    helper = load("artifact_fixture_executor", SOURCE / ".github/scripts/local_acceptance.py")
    root = output / "project"
    # MSVC's linker still rejects build-script paths beyond MAX_PATH. Keep the
    # private Cargo outputs short even when the evidence directory is nested.
    target = Path(tempfile.mkdtemp(prefix="scode-artifact-")) if os.name == "nt" else output / "target"
    (output / "build-context.json").write_text(json.dumps({
        "cargo_target_directory": str(target),
        "retained_for_diagnostics": True,
        "evidence_directory": str(output),
    }, indent=2) + "\n")
    root.mkdir()
    commands = []

    def execute(arguments, cwd, log, extra=None):
        record = helper.run_command(arguments, cwd, log, {"CARGO_TARGET_DIR": str(target),
            "CARGO_BUILD_JOBS": "1", "CARGO_INCREMENTAL": "0", "CARGO_PROFILE_DEV_DEBUG": "0",
            "CARGO_PROFILE_TEST_DEBUG": "0", **(extra or {})})
        commands.append(record)
        return record

    def command(*arguments, extra=None):
        return execute(arguments, root, output / f"{len(commands):03d}.log", extra)

    compiler_environment = windows_compiler_environment(output, execute)

    def write(relative, contents):
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents, encoding="utf-8")

    write("rust/Cargo.toml", '[workspace]\nmembers=["crates/*"]\nresolver="2"\n')
    write("rust/crates/rusty-sudocode-cli/Cargo.toml", '[package]\nname="rusty-sudocode-cli"\nversion="0.0.1"\nedition="2021"\n[[bin]]\nname="scode"\npath="src/main.rs"\n')
    write("rust/crates/rusty-sudocode-cli/build.rs", (SOURCE / "rust/crates/rusty-sudocode-cli/build.rs").read_text())
    write("rust/crates/rusty-sudocode-cli/src/main.rs", r'''fn main() {
    let structured = std::env::args().nth(1).as_deref() == Some("version");
    if std::env::var_os("SCODE_TRACE_STARTUP").is_some() {
        eprintln!("startup phase=console elapsed_us=1 cumulative_us=1");
        eprintln!("startup phase=clap elapsed_us=1 cumulative_us=2");
        if structured { eprintln!("startup phase=version_output elapsed_us=1 cumulative_us=3"); }
    }
    if structured {
        let target = std::env::var("FIXTURE_TARGET").unwrap_or_else(|_| env!("TARGET").to_string());
        println!(r#"{{"git_sha":"{}","target":"{}","executable_path":{:?}}}"#,
            env!("GIT_SHA"), target, std::env::current_exe().unwrap().to_string_lossy());
    } else { println!("profile={}", if cfg!(debug_assertions) { "debug" } else { "release" }); }
}
''')
    write("rust/crates/rusty-sudocode-cli/tests/pty_model_stream_close.rs", r'''#[test]
fn raw_stream_capture_keeps_streaming_and_resume_responsive() {
    use std::io::Write;
    assert!(option_env!("CARGO_BIN_EXE_scode").is_none(), "standalone driver must not build another CLI");
    let binary = std::env::var_os("SCODE_TEST_BIN").unwrap();
    let child = std::process::Command::new(binary).arg("--version").output().unwrap();
    assert!(child.status.success());
    assert_eq!(String::from_utf8(child.stdout).unwrap().trim(), "profile=release");
    if let Some(path) = std::env::var_os("FIXTURE_MUTATE_ARTIFACT") {
        std::fs::OpenOptions::new().append(true).open(path).unwrap().write_all(b"changed").unwrap();
    }
}
#[test]
fn unselected_case_must_not_run() { panic!("artifact gate widened its exact workflow"); }
''')
    write("rust/crates/scode-benchmark-drivers/Cargo.toml", '[package]\nname="scode-benchmark-drivers"\nversion="0.0.1"\nedition="2021"\nautotests=false\n[[test]]\nname="release_stream_capture"\npath="../rusty-sudocode-cli/tests/pty_model_stream_close.rs"\ntest=false\n')
    for name in ["run.py", "reference.rs"]:
        write("e2e/startup/" + name, (SOURCE / "e2e/startup" / name).read_text())
    write("e2e/terminal-resize/setup_windows.py",
          (SOURCE / "e2e/terminal-resize/setup_windows.py").read_text())
    write("e2e/compiler-context/run.py", '''import argparse, hashlib, json, os, subprocess
from pathlib import Path
p=argparse.ArgumentParser(); p.add_argument('--engine',type=Path); p.add_argument('--directory',type=Path); a=p.parse_args()
assert subprocess.check_output([str(a.engine),'--version'],text=True).strip()=='profile=release'
a.directory.mkdir(parents=True); (a.directory/'actual-engine.json').write_text(json.dumps({'sha256':hashlib.sha256(a.engine.read_bytes()).hexdigest()}))
if os.environ.get('FIXTURE_FAIL_CONTEXT'): raise SystemExit(17)
''')
    command("git", "init", "--initial-branch=main")
    command("git", "config", "user.name", "Native artifact fixture")
    command("git", "config", "user.email", "artifact@example.invalid")
    if os.name == "nt":
        configured = subprocess.run(["git", "config", "--get", "localAcceptance.vsdevcmd"], cwd=SOURCE,
                                    capture_output=True, text=True)
        if configured.returncode == 0 and configured.stdout.strip():
            command("git", "config", "localAcceptance.vsdevcmd", configured.stdout.strip())
    command("cargo", "generate-lockfile", "--offline", "--manifest-path", "rust/Cargo.toml")
    command("git", "add", ".")
    command("git", "-c", "commit.gpgsign=false", "commit", "-m", "Native artifact source")
    command("cargo", "build", "--locked", "--release", "--manifest-path", "rust/Cargo.toml", "-p", "rusty-sudocode-cli", "--bin", "scode")
    binary = target / "release" / ("scode.exe" if os.name == "nt" else "scode")
    original = hashlib.sha256(binary.read_bytes()).hexdigest()
    entry = SOURCE / ".github/scripts/release_artifact_acceptance.py"
    results = {}
    for mode, extra in [("pass", {}), ("wrong-target", {"FIXTURE_TARGET": "wrong-target"}),
                        ("context-failure", {"FIXTURE_FAIL_CONTEXT": "1"}),
                        ("mutated-original", {})]:
        artifact = binary
        if mode == "mutated-original":
            artifact = output / ("mutable-scode.exe" if os.name == "nt" else "mutable-scode")
            shutil.copy2(binary, artifact)
            extra = {"FIXTURE_MUTATE_ARTIFACT": str(artifact)}
        destination = output / mode
        failed = False
        try:
            command(sys.executable, str(entry), "--repository", str(root), "--binary", str(artifact),
                    "--output", str(destination), extra=extra)
        except RuntimeError:
            failed = True
        report = json.loads((destination / "report.json").read_text())
        assert failed == (mode != "pass") and report["status"] == ("passed" if mode == "pass" else "failed"), report
        if mode == "pass":
            assert report["binary_sha256"] == original
            assert not any(row["command"][:3] == ["cargo", "build", "--release"] for row in report["commands"])
            assert (destination / "compiler-context/actual-engine.json").is_file()
            if os.name == "nt":
                preparation = [row for row in report["commands"]
                               if "e2e/terminal-resize/setup_windows.py" in row["command"]]
                assert len(preparation) == 1
                terminal = destination / "native/native-release/000.log"
                assert "NATIVE_CONPTY_HOST=" in terminal.read_text()
                assert (target / "debug/deps/conpty.dll").is_file()
                assert (target / "debug/deps/OpenConsole.exe").is_file()
        results[mode] = {"status": report["status"], "error": report.get("error")}
    command("git", "-c", "commit.gpgsign=false", "commit", "--allow-empty", "-m", "Later source rejects an older executable")
    try:
        command(sys.executable, str(entry), "--repository", str(root), "--binary", str(binary), "--output", str(output / "stale"))
        raise AssertionError("Stale native release was accepted")
    except RuntimeError:
        report = json.loads((output / "stale/report.json").read_text())
        assert "source differs" in report["error"]
        results["stale-source"] = {"status": report["status"], "error": report["error"]}
    assert hashlib.sha256(binary.read_bytes()).hexdigest() == original
    scope_controls = {}
    for index, (relative, expected) in enumerate([
            ("docs/note.md", False), (".github/scripts/release_artifact_acceptance.py", True),
            (".github/actions/release-artifact/msvc.ps1", True), (".github/workflows/release.yml", True)]):
        base = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
        write(relative, "changed fixture input\n")
        command("git", "add", ".")
        command("git", "-c", "commit.gpgsign=false", "commit", "-m", f"Artifact scope {index}")
        head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
        event, selected = output / f"scope-{index}.json", output / f"scope-{index}.txt"
        event.write_text(json.dumps({"pull_request": {"base": {"sha": base}, "head": {"sha": head}}}))
        command(sys.executable, str(SOURCE / ".github/scripts/ci_input_scope.py"),
                extra={"GITHUB_EVENT_NAME": "pull_request", "GITHUB_EVENT_PATH": str(event), "GITHUB_OUTPUT": str(selected)})
        assert f"release_artifact_required={str(expected).lower()}" in selected.read_text()
        scope_controls[relative] = expected
    record = {"status": "PASS", "controls": results, "commands": commands,
              "cargo_target_directory": str(target), "binary_sha256": original,
              "windows_compiler_environment": compiler_environment,
              "real_ci_input_scope": scope_controls,
              "workflow_gate_before_packaging": workflow_contract(),
              "scope": "Real Git/Cargo/native entrypoint controls, not product performance or provider coverage."}
    (output / "result.json").write_text(json.dumps(record, indent=2) + "\n")
    print(json.dumps({key: value for key, value in record.items() if key != "commands"}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    run(parser.parse_args().output.resolve())
