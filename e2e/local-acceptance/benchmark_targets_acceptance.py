"""Run both production benchmark builders with real Cargo and native drivers.

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


def standalone_layout(command):
    """Inspect the real Cargo package, not only this fixture's tiny workspace."""
    log = command("cargo", "metadata", "--no-deps", "--locked", "--format-version=1",
                  "--manifest-path", str(SOURCE / "rust/Cargo.toml"), cwd=SOURCE)
    metadata = json.loads(log.read_text())
    package = next(row for row in metadata["packages"] if row["name"] == "scode-benchmark-drivers")
    expected = {f"benchmark_{name}": SOURCE / f"rust/crates/rusty-sudocode-cli/tests/pty_{name}.rs"
                for name in ["render_performance", "hyperlinks", "prompt_viewport"]}
    assert {row["name"] for row in package["targets"]} == set(expected), "The driver package gained an extra target"
    for row in package["targets"]:
        assert row["kind"] == ["test"] and not row["test"], "Benchmark drivers must be opt-in tests only"
        assert Path(row["src_path"]).resolve() == expected[row["name"]].resolve(), "Driver sources diverged"
        assert not any(other["name"] == row["name"] for other_package in metadata["packages"]
                       if other_package["id"] != package["id"] for other in other_package["targets"]), "Driver names collide"
    return {"package": package["name"], "targets": sorted(expected), "normal_acceptance_disabled": True}


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
    """Drive the paired evaluator and declared schedule with real native processes."""
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
        else if mode == "incomplete" && case == "heavy" { "incomplete" }
        else if mode == "regressed" && side == "candidate" && ["long", "heavy"].contains(&case.as_str()) { "regressed" }
        else { "base" };
    let bytes = fs::read(root.join(format!("{case}-{selected}.json"))).unwrap();
    fs::write(report, bytes).unwrap();
}

#[test]
#[ignore]
fn browser_terminal_links_and_interaction() {
    use std::{env, fs, io::Write, path::Path, process::Command};
    let root = env::var("SCODE_ACCEPTANCE_RENDER_FIXTURE").unwrap();
    let root = Path::new(&root);
    let child = Command::new(env::var("SCODE_TEST_BIN").unwrap()).output().unwrap();
    assert!(child.status.success());
    let side = String::from_utf8(child.stdout).unwrap();
    let side = side.trim();
    assert!(side == "base" || side == "candidate");
    let report = env::var("SCODE_GUI_REPORT").unwrap();
    let label = Path::new(&report).parent().unwrap().file_name().unwrap().to_str().unwrap();
    let mut events = fs::OpenOptions::new().create(true).append(true).open(root.join("events.txt")).unwrap();
    writeln!(events, "browser {label} {side}").unwrap();
    let regressed = env::var("SCODE_ACCEPTANCE_RENDER_MODE").unwrap() == "regressed" && side == "candidate";
    let selected = if regressed { "regressed" } else { "base" };
    fs::write(report, fs::read(root.join(format!("browser-{selected}.json"))).unwrap()).unwrap();
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
    assert cases == ["short", "heavy", "foreground", "background"]
    assert policy["cases"]["heavy"] == {"kind": "markdown", "bytes": 12288, "turns": 1}
    legacy_specs = {"long": {"kind": "prose", "bytes": 8192, "turns": 1},
                    "cap": {"kind": "prose", "bytes": 12288, "turns": 1},
                    "markdown": {"kind": "markdown", "bytes": 2300, "turns": 1}}
    plan = {**shared.policy_plan(policy, "core"), "policy_sha256": shared.digest(terminal / "performance-policy.json")}
    assert plan["total_samples"] == 60 and plan["unjudged_warmups"] == 0 and all("cpu_ms" in names and "rss_peak_mib" in names
                                               for names in plan["metrics"].values())
    for case, spec in {**policy["cases"], **legacy_specs}.items():
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
        value["metadata"] = {"observer": "native-fixture-process"}
        del value["metrics"]["cpu_ms"]
        (fixture / f"{case}-incomplete.json").write_text(json.dumps(value))
    arguments = [sys.executable, str(terminal / "benchmark.py"), "--driver", drivers[0],
                 "--base", str(fixture / "base"), "--candidate", str(fixture / "candidate")]
    results = {}
    for mode in ["pass", "regressed", "incomplete", "missing-sample", "extra-sample", "mispaired-sample", "invalid", "metadata", "command"]:
        events = fixture / "events.txt"
        events.unlink(missing_ok=True)
        destination = output / mode

        def measured(*args):
            if mode == "command":
                return command(sys.executable, "-c", "raise SystemExit(23)")
            log = command(*args, extra={"SCODE_ACCEPTANCE_RENDER_FIXTURE": str(fixture),
                                       "SCODE_ACCEPTANCE_RENDER_MODE": mode})
            if mode in {"missing-sample", "extra-sample", "mispaired-sample"}:
                child_path = Path(args[args.index("--output") + 1]) / "report.json"
                child = json.loads(child_path.read_text())
                if child["policy"]["profiles"]["core"] == ["heavy"]:
                    if mode == "missing-sample":
                        child["samples"].pop()
                    elif mode == "extra-sample":
                        child["samples"].append(dict(child["samples"][0]))
                    else:
                        child["samples"][0]["side"] = child["samples"][1]["side"]
                    child_path.write_text(json.dumps(child))
            return log

        failed = False
        try:
            checks.rendering_cases(terminal, arguments, destination, measured, plan)
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
                assert len(samples) == (24 if case == "short" else 12)
                assert all(row["stage"] in {"aa", "ab"} for row in samples)
                for stage, count, collection in [("aa", 5 if case == "short" else 3, controls),
                                                  ("ab", 7 if case == "short" else 3, comparisons)]:
                    for number in range(count):
                        pair = {row["side"]: row["data"] for row in samples if row["stage"] == stage and row["round"] == number}
                        assert set(pair) == {"base", "candidate"}
                        collection[case].append((pair["base"], pair["candidate"]))
            rows, failures = shared.assess(controls, comparisons, policy, expected_cases)
            assert report["metrics"] == rows and report["failures"] == failures
            assert len(trace) == sum(24 if case == "short" else 12 for case in expected_cases)
            if mode == "regressed":
                assert any("paired increase" in failure for failure in failures), "Relative RSS regression was lost"
        elif mode == "invalid":
            assert len(trace) == 25, "Invalid second scenario did not stop immediately"
        elif mode == "metadata":
            assert len(trace) == 36 and "observer/platform changed" in report["failures"][0]
        elif mode in {"missing-sample", "extra-sample"}:
            assert len(trace) == 36 and "judged observations and no extra warm-ups" in report["failures"][0]
        elif mode == "mispaired-sample":
            assert len(trace) == 36 and "paired sample inventory differs" in report["failures"][0]
        elif mode == "incomplete":
            assert len(trace) == 25 and "missing/extra measurements" in report["failures"][0]
        else:
            assert not trace and "no completed scenario report" in report["failures"][0]
        results[mode] = {"status": report["status"], "native_invocations": len(trace),
                         "completed_cases": report["completed_cases"], "failures": report.get("failures", [])}
    # Keep every judged pair from the old full-round six-scenario control:
    # 144 native calls, excluding its former 12 unjudged warmups. Constant
    # fixture observations must reach
    # the same relative RSS refusal. This does not establish equivalent product
    # coverage for one heavy Markdown workload instead of three independent ones.
    events.unlink(missing_ok=True)
    failed = False
    legacy_policy = output / "legacy-policy.json"
    legacy_cases = ["short", "long", "cap", "markdown", "foreground", "background"]
    legacy_policy.write_text(json.dumps(dict(policy, rounds={"calibration": 5, "comparison": 7}, case_rounds={},
        cases={name: value for name, value in {**policy["cases"], **legacy_specs}.items() if name != "heavy"},
        profiles={"core": legacy_cases, "stress": [*legacy_cases, "repeated"]})))
    try:
        command(*arguments, "--output", str(output / "previous-schedule"),
                "--policy", str(legacy_policy), "--profile", "core",
                extra={"SCODE_ACCEPTANCE_RENDER_FIXTURE": str(fixture), "SCODE_ACCEPTANCE_RENDER_MODE": "regressed"})
    except RuntimeError:
        failed = True
    previous = json.loads((output / "previous-schedule/report.json").read_text())
    revised = json.loads((output / "regressed/report.json").read_text())
    assert failed and previous["status"] == "failed" and len(events.read_text().splitlines()) == 144
    before_rss = next(row for row in previous["metrics"] if row["case"] == "long" and row["metric"] == "rss_peak_mib")
    after_rss = next(row for row in revised["metrics"] if row["case"] == "heavy" and row["metric"] == "rss_peak_mib")
    assert {k:v for k,v in before_rss.items() if k != "case"} == {k:v for k,v in after_rss.items() if k != "case"}
    assert before_rss["result"] == after_rss["result"] == "regressed"
    results["full_scenario_control"] = {"status": previous["status"], "native_invocations": 144,
                                    "same_fixture_rss_decision": True}
    # The real CLI must reject weakening the known short-case sentinel before
    # admitting any process. A changed assertion alone cannot grant a pass.
    events.unlink(missing_ok=True)
    invalid_policy = output / "insufficient-short-rounds.json"
    invalid_policy.write_text(json.dumps(dict(policy, case_rounds={})))
    try:
        command(*arguments, "--output", str(output / "insufficient-short-rounds"),
                "--policy", str(invalid_policy), "--profile", "core",
                extra={"SCODE_ACCEPTANCE_RENDER_FIXTURE": str(fixture), "SCODE_ACCEPTANCE_RENDER_MODE": "pass"})
    except RuntimeError:
        pass
    else:
        raise AssertionError("The short sentinel accepted fewer than five A/A and seven A/B pairs")
    assert not events.exists(), "An undersampled policy admitted a native process"
    results["short_sentinel"] = {"required_judged_observations": 24, "invalid_policy_native_invocations": 0,
                                 "reduced_rounds_refused_before_execution": True}
    # Run the real browser evaluator with native fixture children. Real painted
    # browser/product acceptance remains a separate mandatory profile.
    gui_policy = json.loads((terminal / "gui-policy.json").read_text())
    sampling = gui_policy["sampling"]
    keys = len(sampling["input_delays_ms"])
    gui_samples = {"drag_frame_ms": [1] * sampling["drag_steps"]}
    for phase in ["history", "wait", "stream"]:
        gui_samples[f"input_{phase}_parsed_ms"] = [1] * keys * sampling["parsed_bursts"]
        gui_samples[f"input_{phase}_frame_ms"] = [1] * keys * sampling["painted_bursts"]
    gui_value = {"status": "passed", "idle_bytes": 0, "streaming_selection_preserved": True,
                 "browser": "native-fixture", "xterm": "native-fixture", "environment": {},
                 "metrics": {name: 1 for name in gui_policy["metrics"]}, "samples": gui_samples}
    gui_value["metrics"]["input_history_frame_p95_ms"] = 10
    (fixture / "browser-base.json").write_text(json.dumps(gui_value))
    gui_value["metrics"]["input_history_frame_p95_ms"] = 30
    (fixture / "browser-regressed.json").write_text(json.dumps(gui_value))
    gui_args = [sys.executable, str(terminal / "gui-benchmark.py"), "--driver", drivers[0],
                "--base", str(fixture / "base"), "--candidate", str(fixture / "candidate")]
    for mode in ["pass", "regressed"]:
        events.unlink(missing_ok=True)
        destination = output / ("browser-" + mode)
        failed = False
        try:
            command(*gui_args, "--output", str(destination), extra={
                "SCODE_ACCEPTANCE_RENDER_FIXTURE": str(fixture), "SCODE_ACCEPTANCE_RENDER_MODE": mode})
        except RuntimeError:
            failed = True
        report = json.loads((destination / "report.json").read_text())
        trace = events.read_text().splitlines()
        assert len(trace) == len(report["samples"]) == 12 and report["unjudged_warmups"] == 0
        assert all(row["data"]["samples"] == gui_samples for row in report["samples"])
        assert report["status"] == ("failed" if mode == "regressed" else "passed")
        assert failed == (mode == "regressed")
        if failed:
            assert any("paired increase" in failure for failure in report["failures"])
        assert sum(line.endswith(" candidate") for line in trace) == 3, "A/A must never run the candidate"
        assert "ab-01-candidate candidate" in trace[8] and "ab-01-base base" in trace[9], "Missing alternating pair order"
        results["browser_" + mode] = {"status": report["status"], "native_invocations": len(trace)}
    events.unlink(missing_ok=True)
    weak_gui = output / "weak-browser-policy"
    weak_gui.mkdir()
    for name in ["benchmark.py", "gui-benchmark.py"]:
        shutil.copy2(terminal / name, weak_gui / name)
    (weak_gui / "gui-policy.json").write_text(json.dumps(dict(gui_policy, rounds={"calibration": 2, "comparison": 3})))
    try:
        command(sys.executable, str(weak_gui / "gui-benchmark.py"), *gui_args[2:],
                "--output", str(output / "insufficient-browser-rounds"), extra={
                    "SCODE_ACCEPTANCE_RENDER_FIXTURE": str(fixture), "SCODE_ACCEPTANCE_RENDER_MODE": "pass"})
    except RuntimeError:
        pass
    else:
        raise AssertionError("The browser accepted fewer than three paired controls")
    assert not events.exists(), "An undersampled browser policy admitted a native process"
    results["browser_minimum_rounds_refused"] = True
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
        try:
            row = executor.run_command(args, cwd, log, environment)
        except RuntimeError:
            # Keep expected refusals in the sequence before another stage runs.
            commands.append(json.loads(log.with_suffix(".command.json").read_text()))
            raise
        commands.append(row)
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
for name in ["driver","base","candidate","output"]: p.add_argument("--"+name)
p.add_argument("--describe-policy",action="store_true")
p.add_argument("--policy");p.add_argument("--profile");a=p.parse_args()
pairs=[{"stage":stage,"round":number,"side":side} for stage,count in [("aa",5),("ab",7)]
       for number in range(count) for side in (["base","candidate"] if number%2==0 else ["candidate","base"])]
if a.describe_policy:
    assert a.profile=="core" and json.loads(Path(a.policy).read_text())["fixture"] is True
    print(json.dumps({"cases":["short"],"samples":{"short":24},"total_samples":24,"pairs":{"short":pairs},"unjudged_warmups":0,
        "policy_sha256":hashlib.sha256(Path(a.policy).read_bytes()).hexdigest()}))
    raise SystemExit(0)
assert subprocess.check_output([a.base]).decode().strip()=="baseline:normal"
assert subprocess.check_output([a.candidate]).decode().strip()=="candidate:normal"
case="rendering_driver_runs_release" if a.policy else "browser_terminal_links_and_interaction"
if a.policy: assert a.profile=="core" and json.loads(Path(a.policy).read_text())=={"fixture":True,"rounds":{"calibration":5,"comparison":7},"profiles":{"core":["short"]}}
out=Path(a.output);out.mkdir()
samples=[]
for index in range(24 if a.policy else 1):
    subprocess.run([a.driver,"--ignored","--exact",case],env={**os.environ,"SCODE_TEST_BIN":a.candidate},check=True)
    name=f"fixture-{index:02d}.json"
    data={"metadata":{"observer":"native-builder-fixture"}}
    (out/name).write_text(json.dumps(data))
    samples.append({"path":name,"data":data,**(pairs[index] if a.policy else {})})
(out/"fixture.json").write_text(json.dumps({"candidate_sha256":hashlib.sha256(Path(a.candidate).read_bytes()).hexdigest()}))
if a.policy:
    (out/"report.json").write_text(json.dumps({"policy":json.loads(Path(a.policy).read_text()),
        "policy_sha256":hashlib.sha256(Path(a.policy).read_bytes()).hexdigest(),"status":"passed","samples":samples,"unjudged_warmups":0,
        **{name+"_sha256":hashlib.sha256(Path(getattr(a,name)).read_bytes()).hexdigest() for name in ["base","candidate","driver"]},
        "harness_sha256":{"fixture":"browser-builder"}}))
'''
    for label in ["baseline", "candidate", "trusted"]:
        checkout = inputs / label
        files = {
            "rust/Cargo.toml": '[package]\nname="rusty-sudocode-cli"\nversion="0.0.1"\nedition="2021"\n'
                '[workspace]\nmembers=["support", "crates/scode-benchmark-drivers"]\nresolver="2"\n'
                '[[bin]]\nname="scode"\npath="src/main.rs"\n'
                '[dependencies]\nshared-support={path="support",features=["normal"]}\n'
                '[dev-dependencies]\nshared-support={path="support",features=["dev"]}\n',
            "rust/src/main.rs": f'fn main() {{ println!("{label}:{{}}", shared_support::flavor()); }}\n',
            "rust/support/Cargo.toml": '[package]\nname="shared-support"\nversion="0.0.1"\nedition="2021"\n'
                '[features]\nnormal=[]\ndev=[]\n',
            "rust/support/src/lib.rs": 'pub fn flavor() -> &\'static str { if cfg!(feature="dev") { "dev" } else { "normal" } }\n',
            "rust/crates/scode-benchmark-drivers/Cargo.toml":
                '[package]\nname="scode-benchmark-drivers"\nversion="0.0.1"\nedition="2021"\nautotests=false\n'
                '[dev-dependencies]\nshared-support={path="../../support",features=["normal","dev"]}\n'
                + ''.join(f'[[test]]\nname="benchmark_{name}"\npath="../../tests/pty_{name}.rs"\ntest=false\n'
                          for name in ["hyperlinks", "prompt_viewport", "render_performance"]),
            "rust/tests/pty_hyperlinks.rs": driver.format(name="browser_terminal_links_and_interaction"),
            "rust/tests/pty_prompt_viewport.rs": driver.format(name="viewport_browser_interaction"),
            "rust/tests/pty_render_performance.rs": driver.format(name="rendering_driver_runs_release"),
            "e2e/terminal-resize/package.json": '{"name":"browser-builder-acceptance","version":"1.0.0"}\n',
            "e2e/terminal-resize/package-lock.json": '{"name":"browser-builder-acceptance","version":"1.0.0",'
                '"lockfileVersion":3,"packages":{"":{"name":"browser-builder-acceptance","version":"1.0.0"}}}\n',
            "e2e/terminal-resize/gui-benchmark.py": measurement,
            "e2e/terminal-resize/benchmark.py": measurement,
            "e2e/terminal-resize/performance-policy.json": '{"fixture":true,"rounds":{"calibration":5,"comparison":7},"profiles":{"core":["short"]}}\n',
            "scripts/benchmark_prompt_viewport.py": '''import argparse,hashlib,json,os
from pathlib import Path
import subprocess
p=argparse.ArgumentParser()
for name in ["driver","binary","output"]: p.add_argument("--"+name,required=True)
p.add_argument("--observer",default="headless");a=p.parse_args()
assert subprocess.check_output([a.binary]).decode().strip()=="candidate:normal"
subprocess.run([a.driver,"--ignored","--exact","viewport_browser_interaction"],
    env={**os.environ,"SCODE_TEST_BIN":a.binary},check=True)
out=Path(a.output);out.mkdir()
(out/"fixture.json").write_text(json.dumps({"observer":a.observer,
    "candidate_sha256":hashlib.sha256(Path(a.binary).read_bytes()).hexdigest()}))
''',
        }
        for relative, content in files.items():
            path = checkout / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(content, encoding="utf-8")
        if label == "trusted":
            (checkout / "rust/src/main.rs").write_text('compile_error!("Browser must build the candidate, not trusted source");\n')
        command("cargo", "generate-lockfile", "--manifest-path", str(checkout / "rust/Cargo.toml"))

    # Replace only the storage location with private state. Every production
    # command and target/checkout decision executes unmodified; no global cache
    # or another workline's lease is mounted or written.
    checks.benchmark_storage = lambda name: (output / "state" / name, output / "state/target" / name)
    evidence = output / "browser"
    evidence.mkdir()
    locks = output / "locks"
    locks.mkdir()
    with checks.exclusive_benchmark_state(locks):
        checks.benchmark_locked("browser", inputs / "candidate", evidence, "fixture-base", command, output / "unused-target")

    viewport = [output / "commands" / f"{index:03d}.log" for index, row in enumerate(commands)
                if "benchmark_prompt_viewport" in row["command"] and "--no-run" in row["command"]]
    assert len(viewport) == 1
    artifacts = [json.loads(line) for line in viewport[0].read_text(encoding="utf-8").splitlines()
                 if line.startswith("{")]
    rows = [row for row in artifacts if row.get("reason") == "compiler-artifact"
            and row["target"]["name"] == "shared_support"]
    assert len(rows) == 1 and rows[0]["fresh"], f"Viewport redundantly rebuilt shared support: {rows}"
    assert not any(row.get("target", {}).get("name") == "scode" for row in artifacts), artifacts
    digest = hashlib.sha256((evidence / "scode-candidate").read_bytes()).hexdigest()
    for relative in ["measurements", "viewport-headless", "viewport-browser"]:
        data = json.loads((evidence / relative / "fixture.json").read_text())
        assert data["candidate_sha256"] == digest, "A driver replaced the production binary"
    candidate_build = next(row["command"] for row in commands if "build" in row["command"]
                           and row["command"][row["command"].index("--manifest-path") + 1].endswith("/candidate/rust/Cargo.toml"))
    candidate_target = Path(candidate_build[candidate_build.index("--target-dir") + 1])
    assert subprocess.check_output([str(candidate_target / "release/scode")]).decode().strip() == "candidate:normal"
    assert len(commands) == 12, "A production build/driver/observer stage was omitted"

    trusted_policy = inputs / "trusted/e2e/terminal-resize/performance-policy.json"
    original_policy = trusted_policy.read_bytes()
    trusted_policy.write_text('{"fixture":false}\n')
    invalid_plan_output = output / "invalid-plan"
    invalid_plan_output.mkdir()
    before = len(commands)
    try:
        with checks.exclusive_benchmark_state(locks):
            checks.benchmark_locked("rendering", inputs / "candidate", invalid_plan_output,
                                    "fixture-base", command, output / "unused-target")
    except RuntimeError:
        pass
    else:
        raise AssertionError("An invalid rendering plan reached its builders")
    finally:
        trusted_policy.write_bytes(original_policy)
    refused = commands[before:]
    assert len(refused) == 1 and "--describe-policy" in refused[0]["command"] and refused[0]["exit_code"] != 0
    assert not any("cargo" in row["command"] or "npm" in row["command"] for row in refused)
    assert not (invalid_plan_output / "scode-candidate").exists()

    # The trusted CLI remains a compile_error tripwire throughout rendering:
    # compiling the driver must never build that unused binary. Exercise the
    # usual rendering -> browser order and normal/dev feature switching too.
    stage_digests = []
    for name in ["rendering", "browser"]:
        if name == "browser":
            # Preserve the measured normal binary from this invocation, then
            # actually compile another workline's source at the shared canonical
            # path before browser acceptance restores its own exact sources.
            identity = {"candidate": "fixture-candidate-A", "policy": "fixture-protected",
                        "image_id": "sha256:" + "1" * 64, "context_sha256": "2" * 64}
            (inputs / "identity.json").write_text(json.dumps(identity))
            retained = inputs / "candidate-release"
            retained.mkdir()
            shutil.copy2(output / "subsequent-rendering/scode-candidate", retained / "scode")
            (retained / "identity.json").write_text(json.dumps({
                **identity, "version": 1, "sha256": digest,
            }))
            cached = output / "state/rendering/candidate/rust"
            (cached / "src/main.rs").write_text('fn main() { println!("other-workline"); }\n')
            command("cargo", "build", "--locked", "--release", "--manifest-path", str(cached / "Cargo.toml"),
                    "--target-dir", str(output / "state/target/rendering/candidate"), "-p", "rusty-sudocode-cli", "--bin", "scode")
            assert subprocess.check_output([str(candidate_target / "release/scode")]).decode().strip() == "other-workline"
        before = len(commands)
        stage = output / ("subsequent-" + name)
        stage.mkdir()
        with checks.exclusive_benchmark_state(locks):
            checks.benchmark_locked(name, inputs / "candidate", stage, "fixture-base", command, output / "unused-target")
        stage_digests += [hashlib.sha256((stage / "scode-candidate").read_bytes()).hexdigest(),
                          json.loads((stage / ("measurements/short/fixture.json" if name == "rendering"
                                              else "measurements/fixture.json")).read_text())["candidate_sha256"]]
        if name == "browser":
            extra_builds = [row for row in commands[before:] if "build" in row["command"]
                            and "--manifest-path" in row["command"]
                            and row["command"][row["command"].index("--manifest-path") + 1].endswith("/candidate/rust/Cargo.toml")]
            assert not extra_builds, "Browser rebuilt the already measured normal release"
            assert len(commands) - before == 8, "A required browser driver/observer was omitted"
    candidate_builds = [(index, row["command"]) for index, row in enumerate(commands)
                        if "build" in row["command"] and "--manifest-path" in row["command"]
                        and row["command"][row["command"].index("--manifest-path") + 1].endswith("/candidate/rust/Cargo.toml")]
    assert len(candidate_builds) == 3
    assert len({args[args.index("--manifest-path") + 1] for _, args in candidate_builds}) == 1, "Candidate source path must stay canonical across profiles"
    assert len({args[args.index("--target-dir") + 1] for _, args in candidate_builds}) == 1, "Candidate outputs must stay with the same source path"
    assert all(value == digest for value in stage_digests), "Production candidate changed between profiles"
    assert subprocess.check_output([str(candidate_target / "release/scode")]).decode().strip() == "other-workline"
    driver_logs = [output / "commands" / f"{index:03d}.log" for index, row in enumerate(commands)
                   if "--message-format=json-render-diagnostics" in row["command"]]
    for log in driver_logs:
        records = [json.loads(line) for line in log.read_text().splitlines() if line.startswith("{")]
        assert not any(row.get("target", {}).get("name") == "scode" for row in records), log
    target_trees = sorted({row["command"][row["command"].index("--target-dir") + 1]
                           for row in commands if "--target-dir" in row["command"]})
    assert len(target_trees) == 4, target_trees
    layout = standalone_layout(command)

    # An older author snapshot still uses its original target. A present but
    # broken new package must fail; it cannot silently fall back to CLI targets.
    legacy = output / "legacy-inputs"
    shutil.copytree(inputs, legacy, ignore=shutil.ignore_patterns("identity.json", "candidate-release"))
    source = legacy / "candidate/rust"
    shutil.rmtree(source / "crates/scode-benchmark-drivers")
    manifest = source / "Cargo.toml"
    manifest.write_text(manifest.read_text().replace(', "crates/scode-benchmark-drivers"', ''))
    command("cargo", "generate-lockfile", "--manifest-path", str(manifest))
    legacy_output = output / "legacy-browser"
    legacy_output.mkdir()
    before = len(commands)
    with checks.exclusive_benchmark_state(locks):
        checks.benchmark_locked("browser", legacy / "candidate", legacy_output, "fixture-base", command, output / "unused-target")
    assert all(row["command"][row["command"].index("-p") + 1] == "rusty-sudocode-cli"
               for row in commands[before:] if "--test" in row["command"])
    assert json.loads((legacy_output / "measurements/fixture.json").read_text())["candidate_sha256"] == digest
    assert subprocess.check_output([str(candidate_target / "release/scode")]).decode().strip() == "candidate:dev"

    broken = source / "crates/scode-benchmark-drivers"
    broken.mkdir(parents=True)
    (broken / "Cargo.toml").write_text('[package]\nname="unexpected-driver-package"\nversion="0.0.1"\nedition="2021"\n')
    (broken / "src").mkdir()
    (broken / "src/lib.rs").write_text('pub fn unused() {}\n')
    manifest.write_text(manifest.read_text().replace('members=["support"]', 'members=["support", "crates/scode-benchmark-drivers"]'))
    command("cargo", "generate-lockfile", "--manifest-path", str(manifest))
    broken_output = output / "broken-browser"
    broken_output.mkdir()
    before = len(commands)
    try:
        with checks.exclusive_benchmark_state(locks):
            checks.benchmark_locked("browser", legacy / "candidate", broken_output, "fixture-base", command, output / "unused-target")
    except RuntimeError:
        pass
    else:
        raise AssertionError("A broken standalone driver package fell back instead of failing")
    refusal_path = output / "commands" / f"{len(commands) - 1:03d}.command.json"
    refusal = json.loads(refusal_path.read_text())
    assert refusal["exit_code"] != 0 and refusal["command"][-2:] == ["--no-run", "--message-format=json-render-diagnostics"]
    assert refusal["command"][refusal["command"].index("-p") + 1] == "scode-benchmark-drivers"
    assert not (broken_output / "measurements").exists(), "An invalid driver admitted product measurement"
    assert not any(row["command"][row["command"].index("-p") + 1] == "rusty-sudocode-cli"
                   for row in commands[before:] if "--test" in row["command"])
    migration = cache_migration(output / "cache-migration", checks, command)
    matrix = rendering_matrix(output / "matrix", checks, command)
    result = {"status": "PASS", "commands": commands, "candidate_sha256": digest,
              "target_trees": target_trees, "cross_profile_candidate_build_reused": True,
              "interleaved_other_candidate_does_not_rebuild_normal_release": True,
              "standalone_drivers_emit_no_cli_binary": True,
              "actual_source_driver_layout": layout,
              "legacy_candidate_supported": True, "broken_standalone_package_refused": True,
              "expected_broken_package_failure": {"record": str(refusal_path), **refusal},
              "invalid_render_plan_refused_before_build": True,
              "rendering_matrix": matrix, "cache_migration": migration,
              "profiles_sha256": hashlib.sha256(profiles.read_bytes()).hexdigest(),
              "scope": "Real production browser build executor, Cargo and npm; distinct baseline/candidate and native integration drivers, reusable dev artifacts, immutable production binary. Tiny infrastructure fixtures, no product browser coverage credit."}
    for index, row in enumerate(commands):
        log = output / "commands" / f"{index:03d}.log"
        assert hashlib.sha256(log.read_bytes()).hexdigest() == row["log_sha256"], "A later stage replaced earlier evidence"
        assert json.loads(log.with_suffix(".command.json").read_text()) == row, "A command record was replaced"
    (output / "result.json").write_text(json.dumps(result, indent=2), encoding="utf-8")
    print(json.dumps({"status": "PASS", "evidence": str(output), "candidate_sha256": digest}), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--profiles", type=Path, default=SOURCE / ".github/local-acceptance/checks.py")
    arguments = parser.parse_args()
    run(arguments.output.resolve(), arguments.profiles.resolve())
