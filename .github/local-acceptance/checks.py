"""Real acceptance commands shared by the author's hook and opposite-OS CI."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
import zipfile


def changed_cli_test_targets(root, base):
    """Scope isolated test edits; any shared or production input keeps the full suite."""
    changed = subprocess.check_output(["git", "diff", "--name-only", "--no-renames", "-z", base, "HEAD"],
                                      cwd=root).decode().split("\0")[:-1]
    # External specs/fixtures can affect unchanged tests too. Only known docs
    # may accompany an isolated integration module without widening acceptance.
    inputs = [path for path in changed if path != "README.md"
              and not (path.startswith("docs/") and path.endswith(".md"))]
    pattern = r"rust/crates/rusty-sudocode-cli/tests/([A-Za-z0-9_]+)\.rs"
    if not inputs or any(not re.fullmatch(pattern, path) or not (root / path).is_file() for path in inputs):
        return None
    return sorted({Path(path).stem for path in inputs})


def run(name, root, output, base, execute):
    output.mkdir(parents=True, exist_ok=True)
    results = []
    rust = root / "rust"
    target = Path(os.environ.get("CARGO_TARGET_DIR", rust / "target")).resolve()
    suffix = ".exe" if os.name == "nt" else ""
    env = {"SCODE_TEST_BACKEND": "mock", "CARGO_TARGET_DIR": str(target)}

    def command(*args, cwd=root, extra=None):
        results.append(execute(args, cwd, output / f"{len(results):03d}.log", {**env, **(extra or {})}))
        return output / f"{len(results)-1:03d}.log"

    def cargo(*args, **kwargs):
        return command("cargo", *args, cwd=rust, **kwargs)

    if name == "preflight":
        command(sys.executable, ".github/scripts/check_doc_source_of_truth.py")
        command(sys.executable, ".github/scripts/check_pr_history.py", base, "HEAD")
    elif name == "guard-contract":
        command(sys.executable, "e2e/local-acceptance/run.py")
    elif name == "native-sdk":
        sdk = root / "sdk/typescript"
        command("node", "--version", cwd=sdk)
        command("npm", "ci", cwd=sdk)
        command("npm", "test", cwd=sdk)
        command("npm", "run", "pack:runtime", cwd=sdk)
    elif name == "native-workspace":
        cargo("fmt", "--all", "--check")
        tests = changed_cli_test_targets(root, base)
        (output / "test-scope.json").write_text(json.dumps({
            "mode": "changed-cli-targets" if tests is not None else "full-workspace",
            "targets": tests,
        }, indent=2))
        if tests is not None:
            arguments = ["test", "--locked", "-p", "rusty-sudocode-cli", "--features", "engine-acp/mailbox"]
            for test in tests:
                arguments += ["--test", test]
            cargo(*arguments)
        else:
            command(sys.executable, "scripts/generate_cache_metrics.py", "--check")
            command(sys.executable, "scripts/check_cache_usage_fixtures.py")
            command(sys.executable, "e2e/build-metadata/run.py",
                    extra={"BUILD_METADATA_EVIDENCE": str(output / "build-metadata")})
            cargo("test", "--locked", "--manifest-path", "vendor/codex-mermaid/Cargo.toml", "--target-dir", str(target))
            cargo("check", "--workspace", "--benches", "--locked")
            cargo("test", "--workspace", "--locked", "--features", "engine-acp/mailbox")
            cargo("clippy", "--workspace", "--locked")
    elif name == "native-resize":
        if os.name == "nt":
            terminal = Path.home() / ".cache/sudocode-terminal-host"
            command(sys.executable, "e2e/terminal-resize/setup_windows.py", str(terminal))
            command(sys.executable, "e2e/terminal-resize/check_windows_setup.py", str(terminal / "host.zip"))
            env.update({"SCODE_TERMINAL_HOST": str(terminal / "Code.exe"),
                        "SCODE_TERMINAL_MODULES": str(terminal / "07f806f999/resources/app/node_modules.asar")})
        else:
            command("npm", "ci", "--prefix", "e2e/terminal-resize")
        env["SCODE_TERMINAL_LOG_DIR"] = str(output / "terminal")
        tests = ["pty_chrome_resize", "pty_prose_preview", "pty_review_scrollback"]
        if os.name != "nt":
            tests.append("pty_codex_colors")
        for test in tests:
            cargo("test", "--locked", "-p", "rusty-sudocode-cli", "--test", test,
                  "--", "--ignored", "--nocapture")
    elif name == "native-release":
        cargo("build", "--release", "--locked", "-p", "rusty-sudocode-cli", "--bin", "scode")
        binary = output / ("scode" + suffix)
        shutil.copy2(target / "release" / ("scode" + suffix), binary)
        env["SCODE_TEST_BIN"] = str(binary)
        cargo("test", "--release", "--locked", "-p", "rusty-sudocode-cli", "--test", "pty_model_stream_close",
              "raw_stream_capture_keeps_streaming_and_resume_responsive", "--", "--exact", "--nocapture")
        command(sys.executable, "e2e/startup/run.py", "--bin", str(binary), "--budget-ms", "100",
                "--report", str(output / "startup.json"))
    elif name == "native-services":
        cargo("build", "--locked", "-p", "rusty-sudocode-cli", "--bin", "scode")
        binary = target / "debug" / ("scode" + suffix)
        command(sys.executable, "e2e/shared-services/run.py", "--scode", str(binary),
                "--artifacts", str(output / "services"))
        # The existing harness resolves the pinned native grpcurl for each OS.
        bash = shutil.which("bash")
        if not bash:
            raise RuntimeError("Git Bash is required for shared-service credential acceptance")
        command(bash, "-c", 'source e2e/nexus-a2a/lib.sh; GRPCURL="$(grpcurl_bin)"; '
                'python e2e/shared-services/existing_box.py --scode "$1" --grpcurl "$(native_path "$GRPCURL")" --artifacts "$2"',
                "scode-services", str(binary), str(output / "existing-box"))
    elif name == "native-cohost":
        # raft-proto rejects protoc >=4. Pin the same tool on macOS/Windows as
        # the release workflow, and leave the developer's global PATH intact.
        if platform.system() in {"Windows", "Darwin"}:
            architecture = "aarch_64" if platform.machine().lower() == "arm64" else "x86_64"
            archive_name = "win64" if os.name == "nt" else f"osx-{architecture}"
            folder = Path.home() / ".cache/sudocode-protoc-3.20.2" / archive_name
            executable = folder / "bin" / ("protoc" + suffix)
            if not executable.exists():
                folder.mkdir(parents=True, exist_ok=True)
                archive = folder / "protoc.zip"
                urllib.request.urlretrieve(f"https://github.com/protocolbuffers/protobuf/releases/download/v3.20.2/protoc-3.20.2-{archive_name}.zip", archive)
                with zipfile.ZipFile(archive) as stream:
                    stream.extractall(folder)
                executable.chmod(0o755)
            env["PROTOC"] = str(executable)
        profile = "cohost-release-windows" if os.name == "nt" else "cohost-release"
        version = "local-" + subprocess.check_output(["git", "rev-parse", "--short=12", "HEAD"], cwd=root).decode().strip()
        env["NEXUSD_BUILD_VERSION"] = version
        cargo("build", "--locked", "--profile", profile, "-p", "nexusd-cohost", "--features",
              "daemon,driver-ai,egress-gate-presidio", "--bin", "nexusd-cohost")
        binary = target / profile / ("nexusd-cohost" + suffix)
        path = command(str(binary), "--version")
        if version not in path.read_text(errors="replace"):
            raise RuntimeError("Co-host build version stamp was lost")
        limits = {"Windows": 42, "Darwin": 45 if platform.machine().lower() == "arm64" else 46, "Linux": 55}
        if binary.stat().st_size > limits[platform.system()] * 1024 * 1024:
            raise RuntimeError("Co-host exceeds the existing platform release size budget")
    elif name in {"rendering", "browser"}:
        if os.environ.get("SCODE_ACCEPTANCE_CONTAINER") == "1":
            benchmark(name, root, output, base, command, target)
        else:
            container(name, root, output, base, command, execute)
    else:
        raise RuntimeError(f"Unknown mandatory acceptance check: {name}")
    if not results:
        raise RuntimeError(f"No command ran for required check: {name}")
    return results


def snapshot(root, ref, destination):
    destination.mkdir(parents=True, exist_ok=False)
    # Extract only Git-tracked inputs; never send target/, private settings or
    # untracked API credentials to Docker. The archive includes no .git data.
    with tempfile.TemporaryFile() as archive:
        subprocess.run(["git", "archive", "--format=tar", ref], cwd=root, stdout=archive, check=True)
        archive.seek(0)
        with tarfile.open(fileobj=archive) as stream:
            stream.extractall(destination, filter="data")


def container(name, root, output, base, command, execute):
    state = Path.home() / ".cache/sudocode-local-acceptance/linux"
    state.mkdir(parents=True, exist_ok=True)
    image = "sudocode-local-acceptance:1"
    # Each checkout has its own Cargo directory; Cargo must never reuse a
    # same-version path crate compiled from another source checkout.
    protected = subprocess.check_output(["git", "rev-parse", "origin/main"], cwd=root).decode().strip()
    context = output / "image-context"
    for source, destination in [
        (".github/local-acceptance/Dockerfile", "Dockerfile"),
        ("e2e/terminal-resize/package.json", "e2e/terminal-resize/package.json"),
        ("e2e/terminal-resize/package-lock.json", "e2e/terminal-resize/package-lock.json"),
    ]:
        path = context / destination
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(subprocess.check_output(["git", "show", f"{protected}:{source}"], cwd=root))
    command("docker", "build", "--tag", image, str(context), extra={"DOCKER_BUILDKIT": "1"})
    policy = json.loads(subprocess.check_output(["git", "show", f"{protected}:e2e/terminal-resize/performance-policy.json"], cwd=root))
    reference = policy["baseline_commit"] if name == "rendering" else base
    sources = output / "inputs"
    sources.mkdir()
    snapshot(root, "HEAD", sources / "candidate")
    snapshot(root, reference, sources / "baseline")
    snapshot(root, protected, sources / "trusted")
    runner = sources / "runner/.github"
    (runner / "local-acceptance").mkdir(parents=True)
    (runner / "scripts").mkdir()
    shutil.copyfile(__file__, runner / "local-acceptance/checks.py")
    shutil.copyfile(execute.__code__.co_filename, runner / "scripts/local_acceptance.py")
    command("docker", "run", "--rm", "--init", "--cpus", "2", "--memory", "8g", "--pids-limit", "2048",
            "--mount", f"type=bind,source={sources},target=/inputs,readonly",
            "--mount", f"type=bind,source={output},target=/evidence",
            "--mount", f"type=bind,source={state},target=/state",
            "--env", "SCODE_ACCEPTANCE_CONTAINER=1", "--env", "CARGO_HOME=/state/cargo", image,
            "/inputs/runner/.github/local-acceptance/checks.py", "--container-check", name,
            "--base", base)


def benchmark(name, root, output, base, command, target):
    # Reuse the fixed existing workload, A/A calibration, paired A/B rounds,
    # budget and sample assertions. Changing execution host changes none of them.
    inputs = Path("/inputs")
    work = Path("/state") / name
    work.mkdir(exist_ok=True)
    for label in ["candidate", "baseline", "trusted"]:
        destination = work / label
        # Sources are disposable copies from this invocation's exact Git archive.
        if destination.exists():
            shutil.rmtree(destination)
        shutil.copytree(inputs / label, destination)
    candidate, baseline, trusted = (work / label for label in ["candidate", "baseline", "trusted"])
    driver_source = trusted if name == "rendering" else candidate
    terminal = driver_source / "e2e/terminal-resize"
    # Dependencies must match the checkout's lock; the image supplies the exact
    # Chromium and its system libraries, not a different JS dependency graph.
    command("npm", "ci", "--prefix", str(terminal))
    driver_name = "pty_render_performance" if name == "rendering" else "pty_hyperlinks"
    for label, source in [("baseline", baseline), ("candidate", candidate)]:
        command("cargo", "build", "--locked", "--release", "--manifest-path", str(source / "rust/Cargo.toml"),
                "--target-dir", f"/state/target/{name}/{label}", "-p", "rusty-sudocode-cli", "--bin", "scode")
        shutil.copy2(f"/state/target/{name}/{label}/release/scode", output / f"scode-{label}")
    log = command("cargo", "test", "--locked", "--release", "--manifest-path", str(driver_source / "rust/Cargo.toml"),
                  "--target-dir", f"/state/target/{name}/driver", "-p", "rusty-sudocode-cli", "--test", driver_name,
                  "--no-run", "--message-format=json-render-diagnostics")
    messages = []
    for line in log.read_text(errors="replace").splitlines():
        if line.startswith("{"):
            messages.append(json.loads(line))
    drivers = [row["executable"] for row in messages if row.get("reason") == "compiler-artifact"
               and row.get("target", {}).get("name") == driver_name and row.get("executable")]
    if len(drivers) != 1:
        raise RuntimeError(f"Expected exactly one compiled {driver_name} driver: {drivers}")
    script = terminal / ("benchmark.py" if name == "rendering" else "gui-benchmark.py")
    args = [sys.executable, str(script), "--driver", drivers[0], "--base", str(output / "scode-baseline"),
            "--candidate", str(output / "scode-candidate"), "--output", str(output / "measurements")]
    if name == "rendering":
        args += ["--policy", str(terminal / "performance-policy.json"), "--profile", "core"]
    command(*args)
    if name == "browser":
        # Move the existing viewport gates too; deleting the old release job
        # must not silently drop its headless/browser CPU and painting checks.
        manifest = str(candidate / "rust/Cargo.toml")
        viewport_target = "/state/target/browser/viewport-driver"
        viewport_env = {"SCODE_TEST_BIN": str(output / "scode-candidate"),
                        "SCODE_VIEWPORT_MEASURE_DIR": str(output / "viewport-paint")}
        log = command("cargo", "test", "--release", "--locked", "--manifest-path", manifest,
                      "--target-dir", viewport_target, "-p", "rusty-sudocode-cli", "--test", "pty_prompt_viewport",
                      "--no-run", "--message-format=json-render-diagnostics", extra=viewport_env)
        records = [json.loads(line) for line in log.read_text(errors="replace").splitlines() if line.startswith("{")]
        drivers = [row["executable"] for row in records if row.get("reason") == "compiler-artifact"
                   and row.get("target", {}).get("name") == "pty_prompt_viewport" and row.get("executable")]
        if len(drivers) != 1:
            raise RuntimeError("Missing viewport measurement driver")
        command("cargo", "test", "--release", "--locked", "--manifest-path", manifest, "--target-dir", viewport_target,
                "-p", "rusty-sudocode-cli", "--test", "pty_prompt_viewport", "viewport_browser_interaction",
                "--", "--ignored", "--exact", "--nocapture", extra=viewport_env)
        for observer in ["headless", "browser"]:
            arguments = [sys.executable, str(candidate / "scripts/benchmark_prompt_viewport.py"),
                         "--driver", drivers[0], "--binary", str(output / "scode-candidate"),
                         "--output", str(output / ("viewport-" + observer))]
            if observer == "browser":
                arguments += ["--observer", "browser"]
            command(*arguments, extra=viewport_env)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--container-check", choices=["rendering", "browser"])
    parser.add_argument("--native-checks", nargs="*")
    parser.add_argument("--base", required=True)
    parser.add_argument("--output", type=Path, default=Path("/evidence/container"))
    args = parser.parse_args()
    runner = Path(__file__).resolve().parents[2]
    root = Path("/inputs/candidate") if args.container_check else runner
    spec = importlib.util.spec_from_file_location("local_acceptance", runner / ".github/scripts/local_acceptance.py")
    helper = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(helper)
    names = [args.container_check] if args.container_check else args.native_checks
    for name in names:
        results = run(name, root, args.output / name, args.base, helper.run_command)
        (args.output / name / "commands.json").write_text(json.dumps(results, indent=2))


if __name__ == "__main__":
    main()
