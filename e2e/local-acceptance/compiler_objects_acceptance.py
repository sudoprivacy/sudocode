"""Exercise pinned compiler objects through Cargo, nextest and a real PTY."""

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import sys


SOURCE = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def run(output):
    output.mkdir(parents=True, exist_ok=False)
    helper = load("compiler_object_runner", SOURCE / ".github/scripts/local_acceptance.py")
    helper.ROOT = SOURCE
    checks = load("compiler_object_checks", SOURCE / ".github/local-acceptance/checks.py")
    cache = load("compiler_object_cache", SOURCE / ".github/local-acceptance/compiler_cache.py")
    # Respect the caller's declared tools/settings in production. This fixture
    # owns only its child environment and private archives/object/output roots.
    ambient = dict(os.environ)
    for key in list(os.environ):
        if key.upper().startswith("SCCACHE_") or key.upper() in {
            "RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER", "CARGO_TARGET_DIR", "CARGO_BUILD_BUILD_DIR"
        }:
            os.environ.pop(key)
    try:
        if os.name == "nt":
            # Windows known-folder discovery still evaluates the standard
            # profile directories before reading an explicit sccache config.
            for relative in ("AppData/Local", "AppData/Roaming"):
                (Path.home() / relative).mkdir(parents=True, exist_ok=True)
        tool = cache.prepare(checks.github_asset_download)
        if tool is None:
            raise RuntimeError("The compiler-object fixture requires a pinned supported platform")
        nextest = checks.nextest_binary(output)
        workspaces = [output / name for name in ("source-a", "source-b")]
        manifest = Path(__file__).with_name("compiler-objects.Cargo.toml").read_text(encoding="utf-8")
        main = '''fn main() {
    assert_eq!(memchr::memchr(b'x', b"xy"), Some(0));
    assert!(option_env!("CARGO_TARGET_DIR").is_some(), "preserve compiler environment");
    if let Some(cap) = option_env!("SCODE_ACCEPTANCE_BUILD_JOBS") {
        let requested: usize = cap.parse().unwrap();
        let actual: usize = env!("CARGO_BUILD_JOBS").parse().unwrap();
        assert!(actual <= requested, "the executor exceeded its explicit compiler-job cap");
    }
    println!("MARKER:{}:{}", env!("CACHE_COMPILE_INPUT"), cfg!(feature="enabled"));
}
'''
        tests = r'''use std::path::Path;
use std::process::Command;

fn normal_priority() {
    #[cfg(windows)] {
        #[link(name="kernel32")]
        unsafe extern "system" { fn GetCurrentProcess() -> *mut std::ffi::c_void;
            fn GetPriorityClass(process: *mut std::ffi::c_void) -> u32; }
        assert_eq!(unsafe { GetPriorityClass(GetCurrentProcess()) }, 0x20);
    }
    #[cfg(unix)] {
        unsafe extern "C" { fn getpriority(which: i32, who: u32) -> i32; }
        assert_eq!(unsafe { getpriority(0, 0) }, 0);
    }
}

#[test]
fn real_pty_and_driver_layout() {
    normal_priority();
    let target = Path::new(env!("CARGO_TARGET_DIR")).canonicalize().unwrap();
    assert!(std::env::current_exe().unwrap().canonicalize().unwrap().starts_with(&target));
    // nextest puts the target's legacy `debug/deps` on the library search path,
    // which is where the terminal installer places the pinned ConPTY drivers.
    #[cfg(windows)] {
        let drivers = target.join("debug").join("deps");
        let search = std::env::var_os("PATH").unwrap();
        assert!(std::env::split_paths(&search).any(|dir| dir.canonicalize().ok().as_ref() == Some(&drivers)));
        for sidecar in ["conpty.dll", "OpenConsole.exe"] {
            assert!(drivers.join(sidecar).is_file());
        }
    }
    let bin = env!("CARGO_BIN_EXE_compiler-object-consumer");
    let mut session = pty_expect::PtySession::spawn(bin, &[]).unwrap();
    session.set_default_timeout(std::time::Duration::from_secs(15));
    session.expect(&format!("{}:{}:{}", "MARKER", env!("CACHE_COMPILE_INPUT"), cfg!(feature="enabled"))).unwrap();
    assert_eq!(session.expect_eof().unwrap(), 0);
}

#[test]
fn nested_cargo_and_explicit_target() {
    normal_priority();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let inherited = std::env::var("CARGO_TARGET_DIR").expect("preserve test environment");
    assert_eq!(Path::new(env!("CARGO_TARGET_DIR")), Path::new(&inherited));
    for explicit in [false, true] {
        let target = if explicit { root.join("nested-output") } else { Path::new(&inherited).to_path_buf() };
        let mut command = Command::new(env!("CARGO"));
        command.args(["run", "--offline", "--manifest-path"]).arg(root.join("nested/Cargo.toml"));
        if explicit { command.arg("--target-dir").arg(&target); }
        let result = command.output().unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        assert_eq!(String::from_utf8(result.stdout).unwrap().trim(), "NESTED:alpha");
        assert!(target.join("debug").is_dir());
    }
}
'''
        for index, workspace in enumerate(workspaces):
            for folder in ("src", "tests", "nested/src"):
                (workspace / folder).mkdir(parents=True)
            (workspace / "Cargo.toml").write_text(manifest, encoding="utf-8")
            (workspace / "src/main.rs").write_text(main.replace("MARKER", f"SOURCE{index}"), encoding="utf-8")
            (workspace / "tests/process.rs").write_text(tests.replace("MARKER", f"SOURCE{index}"), encoding="utf-8")
            (workspace / "nested/Cargo.toml").write_text(
                '[workspace]\n[package]\nname="nested-consumer"\nversion="0.0.0"\nedition="2021"\n', encoding="utf-8")
            (workspace / "nested/src/main.rs").write_text(
                'fn main() {println!("NESTED:{}", env!("CACHE_COMPILE_INPUT"));}\n', encoding="utf-8")
            shutil.copyfile(Path(__file__).with_name("compiler-objects.Cargo.lock"), workspace / "Cargo.lock")
        rows, sessions = [], []

        def command(label, argv, cwd, env=None, execute=None, expected=0):
            log = output / (label + ".log")
            try:
                (execute or helper.run_command)(argv, cwd, log, env)
            except RuntimeError:
                if expected == 0:
                    raise
            receipt = json.loads(log.with_suffix(".command.json").read_text(encoding="utf-8"))
            assert receipt["exit_code"] == expected, receipt
            rows.append({"label": label, "log": str(log), "exit_code": receipt["exit_code"]})
            return log.read_text(encoding="utf-8")

        for index, workspace in enumerate(workspaces):
            target = output / f"target-{index}"
            with cache.session(tool, output / f"server-{index}", helper.run_command, output / "objects") as active:
                env = {"CACHE_COMPILE_INPUT": "alpha", "CARGO_TARGET_DIR": str(target)}
                build = ["cargo", "build", "--offline", "--locked"]
                command(f"build-{index}", build, workspace, env, active.execute)
                assert (target / "CACHEDIR.TAG").is_file(), "Cargo must initialize its own cache directory"
                binary = target / "debug" / ("compiler-object-consumer.exe" if os.name == "nt" else "compiler-object-consumer")
                assert command(f"binary-{index}", [str(binary)], workspace).strip() == f"SOURCE{index}:alpha:false"
                assert target.resolve().is_relative_to(output.resolve())
                # Retire only this private fixture dependency, then ask real
                # Cargo to compile it again with the original environment.
                command(f"private-dependency-retirement-{index}",
                        ["cargo", "clean", "--package", "memchr", "--target-dir", str(target)], workspace)
                command(f"same-layout-cache-hit-{index}", build, workspace, env, active.execute)
                stats = json.loads(active.tool_command("same-layout-stats", "--show-stats", "--stats-format", "json"))
                assert sum(stats["stats"]["cache_hits"]["counts"].values()) > 0, stats
                if os.name == "nt":
                    command(f"terminal-{index}", [sys.executable, "e2e/terminal-resize/setup_windows.py",
                            str(Path.home() / ".cache/sudocode-terminal-host"), "--native-target", str(target)], SOURCE)
                    checks.verify_native_terminal(output / f"terminal-{index}.log", target)
                command(f"nextest-{index}", [nextest, "nextest", "run", "--offline", "--locked", "--no-tests=fail"],
                        workspace, env, active.execute)
                if index == 1:
                    # Cargo passes arguments that do not fit inline as one
                    # argument file; force that for every unit. The wrapper gives
                    # rustc and clippy-driver the file unexpanded, outside the cache.
                    def cached(label):
                        stats = json.loads(active.tool_command(label, "--show-stats", "--stats-format", "json"))["stats"]
                        return sum(sum(stats[key]["counts"].values()) for key in ("cache_hits", "cache_misses"))
                    before = cached("argument-file-before")
                    forced = {**env, "__CARGO_TEST_FORCE_ARGFILE": "1"}
                    command("argument-file-retirement",
                            ["cargo", "clean", "--package", "memchr", "--target-dir", str(target)], workspace)
                    command("argument-file-build", build, workspace, forced, active.execute)
                    assert command("argument-file-binary", [str(binary)], workspace).strip() == "SOURCE1:alpha:false"
                    built = active.argument_file_units()
                    command("argument-file-lint", ["cargo", "clippy", "--offline", "--locked", "--all-targets"],
                            workspace, forced, active.execute)
                    units = active.argument_file_units()
                    assert built["cargo"] > 0 and units["cargo"] > built["cargo"] and units["wrapper"] == 0, units
                    assert cached("argument-file-after") == before, "argument-file units reached the cache"
                    with cache.session(tool, output / "busy-cache", helper.run_command, output / "objects") as busy:
                        command("busy-cache-original", build, workspace, env, busy.execute)
                    assert busy.disabled and busy.server is None and not busy.tool_commands
                    command("feature", build + ["--features", "enabled"], workspace, env, active.execute)
                    assert command("feature-binary", [str(binary)], workspace).strip() == "SOURCE1:alpha:true"
                    command("compile-environment", build, workspace, {**env, "CACHE_COMPILE_INPUT": "beta"}, active.execute)
                    assert command("environment-binary", [str(binary)], workspace).strip() == "SOURCE1:beta:false"
                    custom = output / "explicit-target"
                    command("explicit-target", build, workspace, {**env, "CARGO_TARGET_DIR": str(custom)}, active.execute)
                    explicit = custom / "debug" / binary.name
                    assert command("explicit-binary", [str(explicit)], workspace).strip() == "SOURCE1:alpha:false"
                    (workspace / "src/main.rs").write_text('compile_error!("deliberate-source-refusal");\n', encoding="utf-8")
                    assert "deliberate-source-refusal" in command("source-refusal", build, workspace, env, active.execute, expected=101)
                    (workspace / "src/main.rs").write_text(main.replace("MARKER", "REPAIRED"), encoding="utf-8")
                    command("source-repaired", build, workspace, env, active.execute)
                    assert command("repaired-binary", [str(binary)], workspace).strip() == "REPAIRED:alpha:false"
            sessions.append({"server": active.server, "priorities": active.priorities,
                             "stats": json.loads((active.directory / "final-stats.log").read_text(encoding="utf-8"))})
        assert sum(sessions[1]["stats"]["stats"]["cache_hits"]["counts"].values()) > 0, sessions
        assert all(session["stats"]["max_cache_size"] == 2 * 1024**3 and session["priorities"]
                   for session in sessions), sessions
        # A session signs {compiler name: {priority: distinct children}}.
        assert all(set(counts) == {str(16384 if os.name == "nt" else 10)} and all(count > 0 for count in counts.values())
                   for session in sessions for counts in session["priorities"].values()), sessions
        for session in sessions:
            import psutil

            assert not psutil.pid_exists(session["server"]["pid"]) or psutil.Process(session["server"]["pid"]).create_time() != session["server"]["birth"]
        # Exercise the public preparation + dispatcher against a real example
        # crate, rather than qualifying only the cache class in isolation.
        os.environ.setdefault("CARGO_HOME", str(Path.home() / ".cargo"))
        os.environ.setdefault("RUSTUP_HOME", str(Path.home() / ".rustup"))
        home = output / "dispatcher-home"
        if os.name == "nt":
            for relative in ("AppData/Local", "AppData/Roaming"):
                (home / relative).mkdir(parents=True, exist_ok=True)
        definition = cache.archive_definition()
        archive = home / ".cache/sudocode-local-acceptance/tools/sccache" / cache.VERSION / definition["target"] / definition["filename"]
        archive.parent.mkdir(parents=True)
        shutil.copyfile(tool["archive"], archive)
        os.environ.update(HOME=str(home), USERPROFILE=str(home))
        project = output / "dispatcher-project"
        rust = project / "rust"
        (rust / "examples").mkdir(parents=True)
        (rust / "Cargo.toml").write_text(
            '[workspace]\n[package]\nname="engine-acp"\nversion="0.0.0"\nedition="2021"\n'
            '[features]\nmailbox=[]\n', encoding="utf-8")
        (rust / "examples/cache.rs").write_text('fn main(){println!("REAL_DISPATCHER_EXAMPLE");}\n', encoding="utf-8")
        command("dispatcher-lock", ["cargo", "generate-lockfile", "--offline"], rust)
        for relative in (".github/local-acceptance/checks.py", ".github/local-acceptance/compiler_cache.py"):
            destination = project / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(SOURCE / relative, destination)
        command("dispatcher-repository", ["git", "init", "--initial-branch=main"], project)
        command("dispatcher-index", ["git", "add", "."], project)
        command("dispatcher-policy", ["git", "-c", "user.name=Compiler fixture", "-c", "user.email=fixture@example.invalid", "-c", "commit.gpgsign=false",
                                      "commit", "-m", "Freeze private dispatcher policy"], project)
        # Materialization must use the protected Git blob, not an unrelated
        # candidate companion with the same filename in the working checkout.
        (project / ".github/local-acceptance/compiler_cache.py").write_text(
            'raise AssertionError("candidate companion executed")\n', encoding="utf-8")
        snapshot = output / "dispatcher"
        snapshot.mkdir()
        helper.ROOT = project
        try:
            staged = helper.materialize_profile_module("HEAD", snapshot)
        finally:
            helper.ROOT = SOURCE
        staged.prepare_native_checks(["native-examples"], project, snapshot, helper.run_command)
        original = staged.run("native-examples", project, snapshot / "native-examples", "HEAD", helper.run_command)
        recorded = original[0]["compiler_cache"] if len(original) == 1 else {}
        assert recorded.get("session", {}).get("stats", {}).get("max_cache_size") == 2 * 1024**3, original
        assert recorded["session_sha256"] == hashlib.sha256(json.dumps(
            recorded["session"], sort_keys=True, separators=(",", ":")).encode()).hexdigest(), recorded
        example = rust / "target/debug/examples" / ("cache.exe" if os.name == "nt" else "cache")
        assert command("dispatcher-example", [str(example)], rust).strip() == "REAL_DISPATCHER_EXAMPLE"
        os.environ["RUSTC_WRAPPER"] = ""
        try:
            staged.prepare_native_checks(["native-examples"], project, output / "caller-dispatcher", helper.run_command)
            assert staged.run.prepared_compiler_cache[1] is None
            caller = staged.run("native-examples", project, output / "caller-dispatcher/native-examples", "HEAD", helper.run_command)
            assert len(caller) == 1 and "compiler_cache" not in caller[0]
            assert caller[0]["command"] == original[0]["command"]
        finally:
            os.environ.pop("RUSTC_WRAPPER")
        # An explicit caller wrapper (including an empty value) keeps the exact
        # original callback and does not start or contact a cache server.
        with cache.session(tool, output / "explicit-wrapper", helper.run_command, output / "objects") as explicit:
            command("caller-wrapper", ["cargo", "build", "--offline", "--locked"], workspaces[1],
                    {"RUSTC_WRAPPER": "", "CACHE_COMPILE_INPUT": "alpha", "CARGO_TARGET_DIR": str(output / "target-1")}, explicit.execute)
        assert explicit.server is None and not explicit.tool_commands
        # A crashed parent releases the OS file lock but leaves a live daemon.
        # The retained owner must prevent a second writer without waiting or
        # stopping that server. Cleanup below uses its original exact identity.
        orphan = output / "interrupted-parent"
        orphan.mkdir()
        information = orphan / "input.json"
        information.write_text(json.dumps({"source": str(SOURCE), "tool": tool, "output": str(orphan)}), encoding="utf-8")
        child = orphan / "parent.py"
        child.write_text('''import importlib.util,json,os,sys
from pathlib import Path
data=json.loads(Path(sys.argv[1]).read_text())
def load(name,relative):
 s=importlib.util.spec_from_file_location(name,Path(data["source"])/relative)
 m=importlib.util.module_from_spec(s);s.loader.exec_module(m);return m
helper=load("orphan_helper",".github/scripts/local_acceptance.py")
helper.ROOT=Path(data["source"])
cache=load("orphan_cache",".github/local-acceptance/compiler_cache.py")
root=Path(data["output"])
active=cache.Session(data["tool"],root/"server",helper.run_command,root/"objects")
assert active.start()
(root/"original.json").write_text(json.dumps({"server":active.server,"environment":active.environment}))
os._exit(17)
''', encoding="utf-8")
        command("interrupted-parent", [sys.executable, "-X", f"pycache_prefix={orphan / 'pycache'}",
                                       str(child), str(information)], SOURCE, expected=17)
        original_owner = json.loads((orphan / "original.json").read_text(encoding="utf-8"))
        recovery = cache.Session(tool, orphan / "cleanup", helper.run_command, orphan / "objects")
        recovery.directory.mkdir()
        recovery.environment = original_owner["environment"]
        recovery.server = original_owner["server"]
        try:
            recovery.assert_owner()
            with cache.session(tool, orphan / "blocked", helper.run_command, orphan / "objects") as blocked:
                command("interrupted-owner-original-cargo", ["cargo", "build", "--offline", "--locked"], workspaces[1],
                        {"CACHE_COMPILE_INPUT": "alpha", "CARGO_TARGET_DIR": str(output / "target-1")}, blocked.execute)
            assert blocked.disabled and blocked.server is None and not blocked.tool_commands
            recovery.assert_owner()
        finally:
            recovery.close()
        assert not (orphan / "objects/.server-owner.json").exists()
        bad_archive = output / "corrupt-archive"
        bad_archive.write_bytes(b"deliberately invalid public archive")
        try:
            cache.executable_bytes(bad_archive, definition)
        except RuntimeError as error:
            assert "checksum mismatch" in str(error)
        else:
            raise AssertionError("Corrupt compiler archive was accepted")
        result = {"status": "PASS", "commands": rows, "sessions": sessions,
                  "module_sha256": hashlib.sha256((SOURCE / ".github/local-acceptance/compiler_cache.py").read_bytes()).hexdigest(),
                  "scope": "Real Cargo, nextest, nested Cargo and native PTY; isolated outputs, feature/env/source invalidation. Not product or population latency acceptance."}
        (output / "result.json").write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(result), flush=True)
    finally:
        os.environ.clear()
        os.environ.update(ambient)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    run(parser.parse_args().output.resolve())
