"""Run the rendering matrix and browser smoke builders with real Cargo and native drivers.

Tiny fixture executables exercise compilation, feature unification and binary
identity. This infrastructure acceptance does not claim product browser coverage.
"""

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


def cache_migration(output, checks, command):
    """Preserve a real Cargo build across migration, then reject stale code."""
    output.mkdir()
    state, legacy = output / "state", output / "legacy"
    source = state / "leased-benchmarks-v1/rendering/candidate"
    target = state / "leased-benchmarks-v1/target/rendering/candidate"
    source.mkdir(parents=True)
    (state / "cargo").mkdir()
    (state / "cargo/config.toml").write_text("# private dependency-cache sentinel\n")
    (state / "unrelated.txt").write_text("Do not migrate obsolete cache directories or unrelated files\n")
    (source / "Cargo.toml").write_text('[package]\nname="cache-migration"\nversion="0.0.0"\nedition="2021"\n'
                                      '[[bin]]\nname="cache-migration"\npath="main.rs"\n')
    (source / "main.rs").write_text('fn main() { println!("candidate-A"); }\n')
    command("cargo", "generate-lockfile", "--manifest-path", str(source / "Cargo.toml"))

    def build():
        return command("cargo", "build", "--locked", "--manifest-path", str(source / "Cargo.toml"),
                       "--target-dir", str(target), "--message-format=json-render-diagnostics")

    build()
    binary = target / "debug/cache-migration"
    before = (binary.stat().st_mtime_ns, hashlib.sha256(binary.read_bytes()).hexdigest())
    state.rename(legacy)
    state.mkdir()
    checks.migrate_benchmark_cache(legacy, state)
    assert before == (binary.stat().st_mtime_ns, hashlib.sha256(binary.read_bytes()).hexdigest())
    assert (state / "cargo/config.toml").read_bytes() == (legacy / "cargo/config.toml").read_bytes()
    assert not (state / "unrelated.txt").exists()
    log = build()
    rows = [json.loads(line) for line in log.read_text().splitlines() if line.startswith("{")]
    artifacts = [row for row in rows if row.get("reason") == "compiler-artifact"]
    assert artifacts and all(row["fresh"] for row in artifacts), "Migrated Cargo graph went cold"
    assert before == (binary.stat().st_mtime_ns, hashlib.sha256(binary.read_bytes()).hexdigest())
    # A completed migration never overwrites an active Linux checkout from an
    # older workline still using the read-only legacy cache.
    (legacy / "leased-benchmarks-v1/rendering/candidate/main.rs").write_text('compile_error!("obsolete legacy source");\n')
    marker = state / ".host-cache-migration-v1.json"
    receipt = marker.read_bytes()
    checks.migrate_benchmark_cache(legacy, state)
    assert marker.read_bytes() == receipt
    changed = output / "changed-source.rs"
    changed.write_text('fn main() { println!("candidate-B"); }\n')
    checks.copy_checkout_file(changed, source / "main.rs")
    log = build()
    changed_artifacts = [json.loads(line) for line in log.read_text().splitlines() if line.startswith("{")]
    changed_artifacts = [row for row in changed_artifacts if row.get("reason") == "compiler-artifact"]
    assert changed_artifacts and all(not row["fresh"] for row in changed_artifacts), "Changed source was falsely Cargo-fresh"
    assert subprocess.check_output([str(binary)]).decode().strip() == "candidate-B"
    assert subprocess.check_output([str(legacy / "leased-benchmarks-v1/target/rendering/candidate/debug/cache-migration")]).decode().strip() == "candidate-A"
    # A partially copied destination has no completion receipt. Re-entering
    # repairs it from the same legacy bytes; unrelated files remain untouched.
    marker.unlink()
    (legacy / "leased-benchmarks-v1/rendering/candidate/main.rs").write_bytes(changed.read_bytes())
    binary.unlink()
    (source / "main.rs").write_text('compile_error!("partial copy must be repaired");\n')
    checks.migrate_benchmark_cache(legacy, state)
    assert json.loads(marker.read_text())["status"] == "complete"
    assert subprocess.check_output([str(binary)]).decode().strip() == "candidate-A"
    build()
    assert subprocess.check_output([str(binary)]).decode().strip() == "candidate-B"
    unsafe = output / "unsafe-state"
    unsafe.mkdir()
    (unsafe / "cargo").symlink_to(output / "must-not-write", target_is_directory=True)
    try:
        checks.migrate_benchmark_cache(legacy, unsafe)
        raise AssertionError("An escaped cache root was copied")
    except RuntimeError as error:
        assert "Unexpected benchmark cache root" in str(error)
    assert not (output / "must-not-write").exists() and not (unsafe / marker.name).exists()
    marker.write_text('{"version":1,"status":"incomplete"}\n')
    try:
        checks.migrate_benchmark_cache(legacy, state)
        raise AssertionError("An incomplete migration receipt was accepted")
    except RuntimeError as error:
        assert "Invalid benchmark cache migration receipt" in str(error)
    return {"migrated_graph_fresh": True, "binary_hash_and_mtime_preserved": True,
            "changed_source_recompiled": True, "legacy_binary_unchanged": True,
            "partial_copy_recovered": True, "escaped_root_and_invalid_marker_refused": True}


def rendering_matrix(output, checks, command):
    """Drive the unchanged paired evaluator through real native fixture processes."""
    output.mkdir()
    fixture = output / "fixture"
    fixture.mkdir()
    (fixture / "Cargo.toml").write_text('[package]\nname="render-matrix-fixture"\nversion="0.0.0"\nedition="2021"\n'
        '[[bin]]\nname="release-marker"\npath="main.rs"\n[[test]]\nname="render-driver"\npath="driver.rs"\n')
    (fixture / "main.rs").write_text('fn main() { println!("{}", std::env::current_exe().unwrap().file_stem().unwrap().to_str().unwrap()); }\n')
    (fixture / "driver.rs").write_text(r'''#[test]
#[ignore]
fn render_performance_workload() {
    use std::{env, fs, io::Write, path::Path, process::Command};
    let root = env::var("SCODE_ACCEPTANCE_RENDER_FIXTURE").unwrap();
    let root = Path::new(&root);
    let binary = env::var("SCODE_TEST_BIN").unwrap();
    let child = Command::new(&binary).output().unwrap();
    assert!(child.status.success());
    let side = String::from_utf8(child.stdout).unwrap();
    let side = side.trim();
    assert!(side == "base" || side == "candidate");
    let case = env::var("SCODE_RENDER_CASE").unwrap();
    let report = env::var("SCODE_RENDER_REPORT").unwrap();
    let label = Path::new(&report).parent().unwrap().file_name().unwrap().to_str().unwrap();
    let mut events = fs::OpenOptions::new().create(true).append(true).open(root.join("events.txt")).unwrap();
    writeln!(events, "{case} {label} {side}").unwrap();
    let mode = env::var("SCODE_ACCEPTANCE_RENDER_MODE").unwrap();
    assert!(!(mode == "invalid" && case == "heavy"), "intentional native driver failure");
    let selected = if mode == "metadata" && case == "heavy" { "metadata" }
        else if mode == "regressed" && side == "candidate" && case == "heavy" { "regressed" } else { "base" };
    let bytes = fs::read(root.join(format!("{case}-{selected}.json"))).unwrap();
    fs::write(report, bytes).unwrap();
}
''')
    command("cargo", "generate-lockfile", "--manifest-path", str(fixture / "Cargo.toml"))
    command("cargo", "build", "--locked", "--manifest-path", str(fixture / "Cargo.toml"))
    log = command("cargo", "test", "--locked", "--manifest-path", str(fixture / "Cargo.toml"),
                  "--test", "render-driver", "--no-run", "--message-format=json-render-diagnostics")
    records = [json.loads(line) for line in log.read_text().splitlines() if line.startswith("{")]
    drivers = [row["executable"] for row in records if row.get("reason") == "compiler-artifact"
               and row.get("target", {}).get("name") == "render-driver" and row.get("executable")]
    assert len(drivers) == 1
    for side in ["base", "candidate"]:
        shutil.copy2(fixture / "target/debug/release-marker", fixture / side)
    terminal = SOURCE / "e2e/terminal-resize"
    shared = load("paired_rendering_evaluator", terminal / "benchmark.py")
    policy = json.loads((terminal / "performance-policy.json").read_text())
    cases = policy["profiles"]["core"]
    assert cases[:2] == ["short", "heavy"]
    for case in cases:
        spec = policy["cases"][case]
        phases = ["stream", "history"] if spec["kind"] in {"foreground", "background"} else ["wait", "stream", "history"]
        value = {"version": 1, "status": "complete", "case": case, "spec": spec, "preview": "0",
                 "contracts": policy["contracts"], "turns": [{}] * spec["turns"],
                 "metrics": {name: 1 for name in shared.expected_metrics(spec)},
                 "input_samples_ms": {phase: [1] * (policy["sampling"]["input"] * spec["turns"]) for phase in phases},
                 "action_samples_ms": {"resize": [1] * (policy["sampling"]["resize"] * spec["turns"]),
                     "paste": [1] * spec["turns"], "cancel": [1] * (policy["sampling"]["cancel"] if "wait" in phases else 0)},
                 "metadata": {"observer": "native-fixture-process"}}
        value["metrics"]["rss_peak_mib"] = 10
        (fixture / f"{case}-base.json").write_text(json.dumps(value))
        value["metrics"]["rss_peak_mib"] = 20
        (fixture / f"{case}-regressed.json").write_text(json.dumps(value))
        value["metrics"]["rss_peak_mib"] = 10
        value["metadata"] = {"observer": "different-native-fixture-process"}
        (fixture / f"{case}-metadata.json").write_text(json.dumps(value))
    arguments = [sys.executable, str(terminal / "benchmark.py"), "--driver", drivers[0],
                 "--base", str(fixture / "base"), "--candidate", str(fixture / "candidate")]
    results = {}
    for mode in ["pass", "regressed", "invalid", "metadata", "command"]:
        events = fixture / "events.txt"
        events.unlink(missing_ok=True)
        destination = output / mode

        def measured(*args):
            if mode == "command":
                return command(sys.executable, "-c", "raise SystemExit(23)")
            return command(*args, extra={"SCODE_ACCEPTANCE_RENDER_FIXTURE": str(fixture),
                                        "SCODE_ACCEPTANCE_RENDER_MODE": mode})

        failed = False
        try:
            checks.rendering_cases(terminal, arguments, destination, measured)
        except RuntimeError:
            failed = True
        report = json.loads((destination / "report.json").read_text())
        trace = events.read_text().splitlines() if events.exists() else []
        expected_cases = cases if mode == "pass" else [] if mode == "command" else cases[:2]
        assert {line.split()[0] for line in trace} == set(expected_cases), "An unadmitted scenario ran"
        assert report["policy"] == policy and report["policy_sha256"] == shared.digest(terminal / "performance-policy.json")
        assert all((destination / row["path"]).is_file() for row in report["samples"])
        assert report["status"] == {"pass": "passed", "regressed": "failed"}.get(mode, "invalid")
        assert failed == (mode != "pass"), "A failing matrix returned success"
        complete = cases if mode == "pass" else [] if mode == "command" else ["short"]
        assert report["completed_cases"] == complete and report["remaining_cases"] == [case for case in cases if case not in complete]
        if mode in {"pass", "regressed"}:
            # Replay the unmodified evaluator on exactly the combined observations.
            # It must reach the same verdict/rows as each complete scenario did.
            controls, comparisons = {case: [] for case in expected_cases}, {case: [] for case in expected_cases}
            for case in expected_cases:
                samples = [row for row in report["samples"] if row["data"]["case"] == case]
                assert len(samples) == 26
                for stage, count, collection in [("aa", 5, controls), ("ab", 7, comparisons)]:
                    for number in range(count):
                        pair = {row["side"]: row["data"] for row in samples if row["stage"] == stage and row["round"] == number}
                        assert set(pair) == {"base", "candidate"}
                        collection[case].append((pair["base"], pair["candidate"]))
            rows, failures = shared.assess(controls, comparisons, policy, expected_cases, shared.judged_metrics)
            assert report["metrics"] == rows and report["failures"] == failures
            assert len(trace) == 26 * len(expected_cases)
            if mode == "regressed":
                assert any("paired increase" in failure for failure in failures), "Relative RSS regression was lost"
        elif mode == "invalid":
            assert len(trace) == 27, "Invalid second scenario did not stop immediately"
        elif mode == "metadata":
            assert len(trace) == 52 and "observer/platform changed" in report["failures"][0]
        else:
            assert not trace and "no completed scenario report" in report["failures"][0]
        results[mode] = {"status": report["status"], "native_invocations": len(trace),
                         "completed_cases": report["completed_cases"], "failures": report.get("failures", [])}
    # Control: the prior whole-matrix schedule runs the very same trusted
    # program, drivers and values, and reaches the same RSS failure after every
    # scenario's 26 calls. This observes actual process admission, rather than a path layout.
    events.unlink(missing_ok=True)
    failed = False
    try:
        command(*arguments, "--output", str(output / "previous-schedule"),
                "--policy", str(terminal / "performance-policy.json"), "--profile", "core",
                extra={"SCODE_ACCEPTANCE_RENDER_FIXTURE": str(fixture), "SCODE_ACCEPTANCE_RENDER_MODE": "regressed"})
    except RuntimeError:
        failed = True
    previous = json.loads((output / "previous-schedule/report.json").read_text())
    revised = json.loads((output / "regressed/report.json").read_text())
    assert failed and previous["status"] == "failed" and len(events.read_text().splitlines()) == 26 * len(cases)
    assert [row for row in previous["metrics"] if row["case"] in cases[:2]] == revised["metrics"]
    assert previous["failures"] == revised["failures"], "The schedule changed the decision for identical observations"
    results["previous_schedule"] = {"status": previous["status"], "native_invocations": 26 * len(cases),
                                    "same_observed_verdict_and_rows": True}
    return results


def run(output, profiles):
    if sys.platform != "linux":
        raise RuntimeError("Browser benchmark executors require Linux. Run this acceptance in local Docker or Linux CI.")
    output.mkdir(parents=True, exist_ok=False)
    inputs = output / "inputs"
    checks = load("benchmark_profiles", profiles)
    executor = load("benchmark_executor", SOURCE / ".github/scripts/local_acceptance.py")
    commands = []

    def command(*args, cwd=inputs, extra=None):
        log = output / "commands" / f"{len(commands):03d}.log"
        environment = {"npm_config_audit": "false", "npm_config_fund": "false", **(extra or {})}
        commands.append(executor.run_command(args, cwd, log, environment))
        return log

    driver = '''#[test]
#[ignore]
fn {name}() {{
    let binary = std::env::var("SCODE_TEST_BIN").unwrap();
    let output = std::process::Command::new(binary).output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), "candidate:normal");
    assert_eq!(shared_support::flavor(), "dev");
}}
'''
    measurement = '''import argparse,hashlib,json,os
from pathlib import Path
import subprocess
p=argparse.ArgumentParser()
for name in ["driver","base","candidate","output"]: p.add_argument("--"+name,required=True)
p.add_argument("--policy");p.add_argument("--profile");a=p.parse_args()
assert subprocess.check_output([a.base]).decode().strip()=="baseline:normal"
assert subprocess.check_output([a.candidate]).decode().strip()=="candidate:normal"
assert a.profile=="core" and json.loads(Path(a.policy).read_text())=={"fixture":True,"profiles":{"core":["short"]}}
subprocess.run([a.driver,"--ignored","--exact","rendering_driver_runs_release"],env={**os.environ,"SCODE_TEST_BIN":a.candidate},check=True)
out=Path(a.output);out.mkdir()
(out/"fixture.json").write_text(json.dumps({"candidate_sha256":hashlib.sha256(Path(a.candidate).read_bytes()).hexdigest()}))
(out/"report.json").write_text(json.dumps({"policy":json.loads(Path(a.policy).read_text()),
    "policy_sha256":hashlib.sha256(Path(a.policy).read_bytes()).hexdigest(),"status":"passed","samples":[],
    **{name+"_sha256":hashlib.sha256(Path(getattr(a,name)).read_bytes()).hexdigest() for name in ["base","candidate","driver"]},
    "harness_sha256":{"fixture":"rendering-builder"}}))
'''
    smoke = '''import argparse,hashlib,json,os
from pathlib import Path
import subprocess
p=argparse.ArgumentParser()
for name in ["driver","candidate","output"]: p.add_argument("--"+name,required=True)
a=p.parse_args()
assert subprocess.check_output([a.candidate]).decode().strip()=="candidate:normal"
subprocess.run([a.driver,"--ignored","--exact","browser_terminal_links_and_interaction"],
    env={**os.environ,"SCODE_TEST_BIN":a.candidate},check=True)
out=Path(a.output);out.mkdir()
(out/"fixture.json").write_text(json.dumps({"candidate_sha256":hashlib.sha256(Path(a.candidate).read_bytes()).hexdigest()}))
'''
    for label in ["baseline", "candidate", "trusted"]:
        checkout = inputs / label
        files = {
            "rust/Cargo.toml": '[package]\nname="rusty-sudocode-cli"\nversion="0.0.1"\nedition="2021"\n'
                '[workspace]\nmembers=["support"]\nresolver="2"\n'
                '[[bin]]\nname="scode"\npath="src/main.rs"\n'
                '[dependencies]\nshared-support={path="support",features=["normal"]}\n'
                '[dev-dependencies]\nshared-support={path="support",features=["dev"]}\n',
            "rust/src/main.rs": f'fn main() {{ println!("{label}:{{}}", shared_support::flavor()); }}\n',
            "rust/support/Cargo.toml": '[package]\nname="shared-support"\nversion="0.0.1"\nedition="2021"\n'
                '[features]\nnormal=[]\ndev=[]\n',
            "rust/support/src/lib.rs": 'pub fn flavor() -> &\'static str { if cfg!(feature="dev") { "dev" } else { "normal" } }\n',
            "rust/tests/pty_hyperlinks.rs": driver.format(name="browser_terminal_links_and_interaction"),
            "rust/tests/pty_prompt_viewport.rs": driver.format(name="viewport_browser_interaction"),
            "rust/tests/pty_render_performance.rs": driver.format(name="rendering_driver_runs_release"),
            "e2e/terminal-resize/package.json": '{"name":"browser-builder-acceptance","version":"1.0.0"}\n',
            "e2e/terminal-resize/package-lock.json": '{"name":"browser-builder-acceptance","version":"1.0.0",'
                '"lockfileVersion":3,"packages":{"":{"name":"browser-builder-acceptance","version":"1.0.0"}}}\n',
            "e2e/terminal-resize/gui-smoke.py": smoke,
            "e2e/terminal-resize/benchmark.py": measurement,
            "e2e/terminal-resize/performance-policy.json": '{"fixture":true,"profiles":{"core":["short"]}}\n',
        }
        for relative, content in files.items():
            path = checkout / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content, encoding="utf-8")
        if label == "trusted":
            (checkout / "rust/src/main.rs").write_text('compile_error!("The smoke must build the candidate, not trusted source");\n')
        command("cargo", "generate-lockfile", "--manifest-path", str(checkout / "rust/Cargo.toml"))

    # Replace only the storage location with private state. Every production
    # command and target/checkout decision executes unmodified; no global cache
    # or another workline's lease is mounted or written.
    checks.benchmark_storage = lambda name: (output / "state" / name, output / "state/target" / name)
    evidence = output / "browser"
    evidence.mkdir()
    locks = output / "locks"
    locks.mkdir()
    before = len(commands)
    with checks.exclusive_benchmark_state(locks):
        checks.benchmark_locked("browser", inputs / "candidate", evidence, "fixture-base", command, output / "unused-target")
    smoke_commands = commands[before:]
    assert len(smoke_commands) == 5, "A browser smoke build/driver/run stage was omitted"
    drivers = [row["command"] for row in smoke_commands if "--no-run" in row["command"]]
    assert len(drivers) == 1 and {"pty_hyperlinks", "pty_prompt_viewport"} <= set(drivers[0]), drivers
    assert not any("--release" in row["command"] for row in smoke_commands), "The smoke needs no optimized build"
    digest = hashlib.sha256((evidence / "scode-candidate").read_bytes()).hexdigest()
    assert subprocess.check_output([str(evidence / "scode-candidate")]).decode().strip() == "candidate:normal"
    data = json.loads((evidence / "smoke/fixture.json").read_text())
    assert data["candidate_sha256"] == digest, "A driver replaced the copied normal binary"
    builds = [row["command"] for row in smoke_commands if "build" in row["command"]]
    assert len(builds) == 1 and builds[0][builds[0].index("--manifest-path") + 1].endswith("/browser/candidate/rust/Cargo.toml")
    # The rendering matrix builds the fixed baseline and candidate in release,
    # and its driver from trusted source.
    (inputs / "trusted/rust/src/main.rs").write_text('fn main() {}\n', encoding="utf-8")
    stage = output / "rendering"
    stage.mkdir()
    with checks.exclusive_benchmark_state(locks):
        checks.benchmark_locked("rendering", inputs / "candidate", stage, "fixture-base", command, output / "unused-target")
    rendering = json.loads((stage / "measurements/short/fixture.json").read_text())
    assert rendering["candidate_sha256"] == hashlib.sha256((stage / "scode-candidate").read_bytes()).hexdigest()
    target_trees = sorted({row["command"][row["command"].index("--target-dir") + 1]
                           for row in commands if "--target-dir" in row["command"]})
    assert len(target_trees) == 4, target_trees
    migration = cache_migration(output / "cache-migration", checks, command)
    matrix = rendering_matrix(output / "matrix", checks, command)
    result = {"status": "PASS", "commands": commands, "candidate_sha256": digest,
              "target_trees": target_trees, "rendering_matrix": matrix, "cache_migration": migration,
              "profiles_sha256": hashlib.sha256(profiles.read_bytes()).hexdigest(),
              "scope": "Real production rendering and browser smoke executors, Cargo and npm; distinct baseline/candidate and native integration drivers, immutable production binary. Tiny infrastructure fixtures, no product browser coverage credit."}
    (output / "result.json").write_text(json.dumps(result, indent=2), encoding="utf-8")
    print(json.dumps({"status": "PASS", "evidence": str(output), "candidate_sha256": digest}), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--profiles", type=Path, default=SOURCE / ".github/local-acceptance/checks.py")
    arguments = parser.parse_args()
    run(arguments.output.resolve(), arguments.profiles.resolve())
