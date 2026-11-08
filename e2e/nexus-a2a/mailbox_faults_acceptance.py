"""Exercise batch selection and fail-fast through real Cargo/native processes."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import sys
import time

import psutil

from mailbox_faults import CASES, ROOT


def overlap_controls(source, output, runner, environment):
    """Exercise the production two-lane entry with real Cargo and CLI processes."""
    cli = source / 'cli-stage.sh'
    cli.write_text('#!/usr/bin/env bash\nset -euo pipefail\nexec "$PYTHON" "$(dirname "$0")/cli-probe.py"\n')
    (source / 'cli-probe.py').write_text('''import json,os,time
from pathlib import Path
import psutil
root=Path(os.environ['FAULT_PROBE_ROOT'])
(root/'cli.pid').write_text(json.dumps({'pid':os.getpid(),'created_at':psutil.Process().create_time()}))
(root/'cli.start').write_text(str(time.time_ns()))
if int(os.environ['SCODE_ACCEPTANCE_TEST_THREADS']) > 1:
    deadline=time.monotonic()+20
    while not (root/'__LONG_CASE__.start').exists():
        assert time.monotonic()<deadline, 'native fault control did not start'
        time.sleep(.01)
if os.environ.get('FAULT_PROBE_CLI_FAIL'):
    raise SystemExit(23)
time.sleep(30 if os.environ.get('FAULT_PROBE_FAIL') else .4)
(root/'cli.end').write_text(str(time.time_ns()))
'''.replace('__LONG_CASE__', CASES[-1]))
    rows = []
    original_manifest = (source / 'Cargo.toml').read_text()
    for label, workers, fault, cli_failure, feature in [
        ('overlap-success', 4, '', '', ''),
        ('explicit-one-worker', 1, '', '', ''),
        ('fault-cancels-cli', 4, CASES[0], '', ''),
        ('cli-cancels-faults', 4, '', '1', ''),
        ('overlap-missing', 4, '', '', 'missing'),
        ('overlap-unignored', 4, '', '', 'unignored'),
        ('invalid-cap', 0, '', '', ''),
    ]:
        markers = output / (label + '-markers')
        markers.mkdir()
        if feature:
            (source / 'Cargo.toml').write_text(original_manifest.replace(
                '[features]\n', f'[features]\ndefault=["{feature}"]\n'))
        command = [sys.executable, str(ROOT / 'e2e/nexus-a2a/cli_mailbox_stages.py'),
                   '--manifest', str(source / 'Cargo.toml'), '--output', str(output / label),
                   '--cli-script', str(cli)]
        started = time.monotonic()
        try:
            try:
                runner.run_command(command, source, output / (label + '.log'), {
                    **environment, 'SCODE_ACCEPTANCE_TEST_THREADS': str(workers),
                    'FAULT_PROBE_ROOT': str(markers), 'FAULT_PROBE_FAIL': fault,
                    'FAULT_PROBE_CLI_FAIL': cli_failure, 'PYTHON': sys.executable,
                })
                code = 0
            except RuntimeError:
                code = json.loads((output / (label + '.command.json')).read_text())['exit_code']
                assert code != 0
            elapsed = time.monotonic() - started
            native_starts = [path for path in markers.glob('*.start') if path.stem != 'cli']
            expected_success = label in {'overlap-success', 'explicit-one-worker'}
            assert (code == 0) == expected_success, (label, code)
            if expected_success:
                assert {path.stem for path in native_starts} == set(CASES)
                report = json.loads((output / label / 'result.json').read_text())
                assert report['status'] == 'PASS'
                fault_cap = int(report['command'][report['command'].index('--test-threads') + 1])
                assert fault_cap == max(1, workers - 1)
                events = [(int(path.read_text()), 1) for path in markers.glob('*.start')]
                events += [(int(path.read_text()), -1) for path in markers.glob('*.end')]
                active = maximum = 0
                for _, delta in sorted(events):
                    active += delta
                    maximum = max(maximum, active)
                assert active == 0 and maximum <= workers, (label, maximum)
                if workers > 1:
                    assert maximum > 1
                    assert int((markers / 'cli.start').read_text()) < int((markers / (CASES[-1] + '.end')).read_text())
                else:
                    assert int((markers / 'cli.end').read_text()) < min(int(path.read_text()) for path in native_starts)
            elif label == 'invalid-cap':
                assert not list(markers.glob('*.start'))
            else:
                assert elapsed < 20, (label, elapsed)
                assert not (markers / 'cli.end').exists()
                if feature:
                    assert not native_starts
                else:
                    assert (markers / 'cli.start').exists()
                    assert native_starts and not (markers / (CASES[-1] + '.end')).exists()
            if label != 'invalid-cap':
                stages = output / label / 'stages'
                result = json.loads((stages / 'commands.result.json').read_text())
                assert (result['status'] == 'passed') == expected_success
                assert (stages / 'cli.log').exists() and (stages / 'faults.log').exists()
            identity = markers / 'cli.pid'
            if identity.exists():
                saved = json.loads(identity.read_text())
                try:
                    child = psutil.Process(saved['pid'])
                    assert child.create_time() != saved['created_at'] or not child.is_running(), (label, saved)
                except psutil.NoSuchProcess:
                    pass
            rows.append({'case': label, 'exit_code': code, 'seconds': elapsed,
                         'native_cases_started': len(native_starts), 'worker_cap': workers})
        finally:
            (source / 'Cargo.toml').write_text(original_manifest)
    return rows


def run(output):
    output.mkdir(parents=True, exist_ok=False)
    source = output / "source"
    (source / "src").mkdir(parents=True)
    (source / "tests").mkdir()
    (source / "Cargo.toml").write_text('[package]\nname="runtime"\nversion="0.0.0"\nedition="2021"\n'
                                       '[workspace]\n[features]\nmissing=[]\nunignored=[]\n')
    (source / "src/lib.rs").write_text("")
    body = '''fn record(name: &str) {
    use std::{fs, time::{Duration, SystemTime, UNIX_EPOCH}};
    let root = std::path::PathBuf::from(std::env::var("FAULT_PROBE_ROOT").unwrap());
    let clock = || SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    fs::write(root.join(format!("{name}.start")), clock().to_string()).unwrap();
    let failure = std::env::var("FAULT_PROBE_FAIL").unwrap_or_default();
    if failure == name {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !root.join("__LONG_CASE__.start").exists() {
            assert!(std::time::Instant::now() < deadline, "long native control did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("injected native failure after the long control started");
    }
    let delay = if !failure.is_empty() && name == "__LONG_CASE__" { 30_000 } else { 400 };
    std::thread::sleep(Duration::from_millis(delay));
    fs::write(root.join(format!("{name}.end")), clock().to_string()).unwrap();
}
'''.replace("__LONG_CASE__", CASES[-1])
    for index, name in enumerate(CASES):
        body += ('#[cfg(not(feature="missing"))]\n#[cfg_attr(not(feature="unignored"), ignore)]\n'
                 if index == 0 else '#[ignore]\n')
        body += f'#[test]\nfn {name}() {{ record("{name}"); }}\n'
    (source / "tests/mailbox_nexus_live.rs").write_text(body)
    spec = importlib.util.spec_from_file_location("mailbox_probe_runner", ROOT / ".github/scripts/local_acceptance.py")
    runner = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(runner)
    runner.ROOT = ROOT
    runner.run_command(["cargo", "generate-lockfile", "--offline"], source, output / "lock.log")
    environment = {**(getattr(runner, "MSVC_ENV", None) or {}),
                   "CARGO_TARGET_DIR": str(source / "target"), "SCODE_ACCEPTANCE_TEST_THREADS": "4"}
    bash = "C:/Program Files/Git/bin/bash.exe" if os.name == "nt" else shutil.which("bash")
    assert bash, "Bash is required by the production daemon harness"
    rows = []
    for label, failure, feature in [("success", "", ""), ("failure", CASES[0], ""),
                                    ("missing", "", "missing"), ("unignored", "", "unignored")]:
        markers = output / (label + "-markers")
        markers.mkdir()
        # Cargo's target selection stays real; only this private fixture's
        # feature set changes to produce a missing or non-ignored test.
        if feature:
            manifest = (source / "Cargo.toml").read_text()
            (source / "Cargo.toml").write_text(manifest.replace('[features]\n', f'[features]\ndefault=["{feature}"]\n'))
        command = [bash, "-c", 'set -euo pipefail; . "$1"; interpreter="$(python3_bin)"; '
                   'exec "$interpreter" "$2" --manifest "$3" --output "$4"', "mailbox-fault-probe",
                   str(ROOT / "e2e/nexus-a2a/lib.sh"), str(ROOT / "e2e/nexus-a2a/mailbox_faults.py"),
                   str(source / "Cargo.toml"), str(output / label)]
        started = time.monotonic()
        try:
            runner.run_command(command, source, output / (label + ".log"),
                               {**environment, "FAULT_PROBE_ROOT": str(markers), "FAULT_PROBE_FAIL": failure})
            code = 0
        except RuntimeError:
            code = json.loads((output / (label + ".command.json")).read_text())["exit_code"]
            assert code != 0
        starts = list(markers.glob("*.start"))
        if label == "success":
            assert code == 0 and {p.stem for p in starts} == set(CASES)
            events = [(int(p.read_text()), 1) for p in starts]
            events += [(int(p.read_text()), -1) for p in markers.glob("*.end")]
            active = maximum = 0
            for _, delta in sorted(events):
                active += delta
                maximum = max(maximum, active)
            assert active == 0 and 2 <= maximum <= 4, maximum
        elif label == "failure":
            assert code != 0 and 0 < len(starts) < len(CASES), starts
            assert (markers / (CASES[0] + ".start")).exists()
            assert (markers / (CASES[-1] + ".start")).exists()
            assert not (markers / (CASES[-1] + ".end")).exists()
            assert time.monotonic() - started < 20, "Failed batch waited for the 30-second native control"
        else:
            assert code != 0 and not starts, (label, starts)
        rows.append({"case": label, "exit_code": code, "native_cases_started": len(starts),
                     "seconds": time.monotonic() - started})
        if feature:
            (source / "Cargo.toml").write_text(manifest)
    overlap = overlap_controls(source, output, runner, environment)
    (output / "result.json").write_text(json.dumps({"status": "PASS", "cases": rows, "cli_overlap_controls": overlap,
        "scope": "Real Cargo, pinned nextest and native fixture processes; no daemon/product coverage."}, indent=2) + "\n")
    print("PASS: seven exact native cases; CLI/fault overlap and explicit one-worker serialization; failures cancel peers; missing/non-ignored inventories and invalid caps refused", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    run(parser.parse_args().output.resolve())
