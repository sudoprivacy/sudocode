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


def prepared_refusals(source, output, runner, environment):
    """Refuse real source/binary/metadata drift before any native case starts."""
    driver = output / 'prepared-refusal-driver.py'
    driver.write_text('''import importlib.util,json,os,sys
from pathlib import Path
entry,manifest,output,mode=map(Path,sys.argv[1:])
mode=str(mode)
spec=importlib.util.spec_from_file_location('prepared_fault_subject',entry)
subject=importlib.util.module_from_spec(spec)
spec.loader.exec_module(subject)
token=subject.prepare(manifest,output)
prepared=json.loads((output/'prepared.json').read_text())
original=None
if mode=='source':
    changed=manifest.parent/'tests/mailbox_nexus_live.rs'
    original=changed.read_bytes()
    changed.write_bytes(original+b'\\n// actual source drift after preparation\\n')
    expected='source changed'
elif mode=='binary':
    changed=Path(next(iter(prepared['binary_sha256'])))
    original=changed.read_bytes()
    changed.write_bytes(original+b'actual binary drift')
    expected='binary changed'
elif mode=='metadata':
    changed=output/'binaries-metadata.json'
    changed.write_bytes(changed.read_bytes()+b'\\n')
    expected='metadata changed'
elif mode=='handoff':
    changed=output/'prepared.json'
    changed.write_bytes(changed.read_bytes()+b'\\n')
    expected='handoff changed'
elif mode=='manifest':
    manifest=manifest.with_name('different-Cargo.toml')
    expected='another manifest'
else:
    raise AssertionError(mode)
try:
    try:
        subject.run(manifest,output,prepared_sha=token)
    except RuntimeError as error:
        assert expected in str(error),(mode,str(error))
        assert not list(Path(os.environ['FAULT_PROBE_ROOT']).glob('*.start'))
        assert not (output/'result.json').exists()
        (output/'expected-refusal.json').write_text(json.dumps({'mode':mode,'error':str(error),'native_cases_started':0}))
    else:
        raise AssertionError('Changed preparation was accepted')
finally:
    if original is not None:
        changed.write_bytes(original)
''')
    rows = []
    for mode in ['source', 'binary', 'metadata', 'handoff', 'manifest']:
        markers = output / ('prepared-' + mode + '-markers')
        markers.mkdir()
        field = output / ('prepared-' + mode)
        result = runner.run_command([sys.executable, str(driver), str(ROOT / 'e2e/nexus-a2a/mailbox_faults.py'),
                                     str(source / 'Cargo.toml'), str(field), mode], source,
                                    output / ('prepared-' + mode + '.log'), {
                                        **environment, 'FAULT_PROBE_ROOT': str(markers),
                                        'FAULT_PROBE_FAIL': '', 'FAULT_PROBE_CARGO_LOCK_EXPECTED': '',
                                    })
        refusal = json.loads((field / 'expected-refusal.json').read_text())
        assert refusal['native_cases_started'] == 0
        rows.append({'mode': mode, 'seconds': result['elapsed_seconds'], 'refusal': refusal})
    return rows


def overlap_controls(source, output, runner, environment, bash):
    """Exercise the production two-lane entry with real Cargo and CLI processes."""
    cli = source / 'cli-stage.sh'
    cli.write_text('#!/usr/bin/env bash\nset -euo pipefail\n'
                   'if [ "${FAULT_PROBE_CARGO_LOCK_EXPECTED:-}" = 1 ]; then\n'
                   '  cargo build --locked --offline --manifest-path "$(dirname "$0")/holder/Cargo.toml"\n'
                   'fi\nexec "$PYTHON" "$(dirname "$0")/cli-probe.py"\n')
    defaults = source / 'caller-defaults.sh'
    defaults.write_text('#!/usr/bin/env bash\nset -euo pipefail\nunset CARGO_BUILD_JOBS CARGO_INCREMENTAL '
                        'CARGO_PROFILE_DEV_DEBUG CARGO_PROFILE_TEST_DEBUG NO_COLOR\nexec "$@"\n')
    (source / 'cli-probe.py').write_text('''import json,os,time
from pathlib import Path
import psutil
root=Path(os.environ['FAULT_PROBE_ROOT'])
(root/'cli.pid').write_text(json.dumps({'pid':os.getpid(),'created_at':psutil.Process().create_time()}))
if not (root/'cli.start').exists():
    (root/'cli.start').write_text(str(time.time_ns()))
(root/'cli.environment.json').write_text(json.dumps({name:os.environ.get(name) for name in
    ['CARGO_BUILD_JOBS','CARGO_INCREMENTAL','CARGO_PROFILE_DEV_DEBUG','CARGO_PROFILE_TEST_DEBUG','NO_COLOR']}))
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
    holder = source / 'holder'
    (holder / 'src').mkdir(parents=True)
    (holder / 'Cargo.toml').write_text('[package]\nname="cli-cargo-lock-holder"\nversion="0.0.0"\nedition="2021"\n[workspace]\n')
    (holder / 'src/main.rs').write_text('fn main() {}\n')
    (holder / 'build.rs').write_text('''use std::{env,fs,path::PathBuf,time::{Duration,Instant,SystemTime,UNIX_EPOCH}};
fn main() {
    println!("cargo:rerun-if-env-changed=FAULT_PROBE_CARGO_LOCK_EXPECTED");
    if env::var("FAULT_PROBE_CARGO_LOCK_EXPECTED").as_deref()!=Ok("1") { return; }
    let root=PathBuf::from(env::var("FAULT_PROBE_ROOT").unwrap());
    let clock=SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos().to_string();
    fs::write(root.join("cli.start"),&clock).unwrap();
    fs::write(root.join("cli.lock-held"),&clock).unwrap();
    let start=Instant::now();
    while [__CASES__].iter().any(|name| !root.join(format!("{name}.end")).exists()) {
        assert!(start.elapsed()<Duration::from_secs(10), "prepared native tests waited for Cargo's build lock");
        std::thread::sleep(Duration::from_millis(10));
    }
}
'''.replace('__CASES__', ','.join(json.dumps(name) for name in CASES)))
    runner.run_command(['cargo', 'generate-lockfile', '--offline', '--manifest-path', str(holder / 'Cargo.toml')],
                       source, output / 'holder-lockfile.log', environment)
    runner.run_command(['cargo', 'build', '--locked', '--offline', '--manifest-path', str(holder / 'Cargo.toml')],
                       source, output / 'holder-prewarm.log', environment)
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
        ('existing-output', 4, '', '', ''),
        ('cargo-lock-overlap', 4, '', '', ''),
        ('caller-defaults', 4, '', '', ''),
    ]:
        markers = output / (label + '-markers')
        markers.mkdir()
        if label == 'existing-output':
            (output / label).mkdir()
            (output / label / 'result.json').write_text('original private evidence must stay byte-identical\n')
        if feature:
            (source / 'Cargo.toml').write_text(original_manifest.replace(
                '[features]\n', f'[features]\ndefault=["{feature}"]\n'))
        command = [sys.executable, str(ROOT / 'e2e/nexus-a2a/cli_mailbox_stages.py'),
                   '--manifest', str(source / 'Cargo.toml'), '--output', str(output / label),
                   '--cli-script', str(cli)]
        if label == 'caller-defaults':
            command = [bash, str(defaults), *command]
        started = time.monotonic()
        try:
            try:
                runner.run_command(command, source, output / (label + '.log'), {
                    **environment, 'SCODE_ACCEPTANCE_TEST_THREADS': str(workers),
                    'FAULT_PROBE_ROOT': str(markers), 'FAULT_PROBE_FAIL': fault,
                    'FAULT_PROBE_CLI_FAIL': cli_failure, 'PYTHON': sys.executable,
                    'FAULT_PROBE_CARGO_LOCK_EXPECTED': '1' if label == 'cargo-lock-overlap' else '',
                    'NO_COLOR': '1',
                })
                code = 0
            except RuntimeError:
                code = json.loads((output / (label + '.command.json')).read_text())['exit_code']
                assert code != 0
            elapsed = time.monotonic() - started
            native_starts = [path for path in markers.glob('*.start') if path.stem != 'cli']
            expected_success = label in {'overlap-success', 'explicit-one-worker', 'cargo-lock-overlap', 'caller-defaults'}
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
                caller = json.loads((markers / 'cli.environment.json').read_text())
                assert caller == ({name: None for name in caller} if label == 'caller-defaults' else {
                    'CARGO_BUILD_JOBS': '1', 'CARGO_INCREMENTAL': '0', 'CARGO_PROFILE_DEV_DEBUG': '0',
                    'CARGO_PROFILE_TEST_DEBUG': '0', 'NO_COLOR': '1'}), (label, caller)
                if label == 'cargo-lock-overlap':
                    assert (markers / 'cli.lock-held').exists()
                    assert int((markers / 'cli.lock-held').read_text()) < min(int(path.read_text()) for path in native_starts)
            elif label in {'invalid-cap', 'existing-output'}:
                assert not list(markers.glob('*.start'))
                if label == 'existing-output':
                    assert (output / label / 'result.json').read_text() == 'original private evidence must stay byte-identical\n'
                    assert not (output / (label + '-stages')).exists()
            else:
                assert elapsed < 20, (label, elapsed)
                assert not (markers / 'cli.end').exists()
                if feature:
                    assert not native_starts
                    assert not (markers / 'cli.start').exists()
                else:
                    assert (markers / 'cli.start').exists()
                    assert native_starts and not (markers / (CASES[-1] + '.end')).exists()
            if label not in {'invalid-cap', 'existing-output'} and not feature:
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
    if std::env::var("FAULT_PROBE_CARGO_LOCK_EXPECTED").as_deref()==Ok("1") {
        let deadline=std::time::Instant::now()+Duration::from_secs(5);
        while !root.join("cli.lock-held").exists() {
            assert!(std::time::Instant::now()<deadline, "real CLI Cargo holder did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
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
    overlap = overlap_controls(source, output, runner, environment, bash)
    refusals = prepared_refusals(source, output, runner, environment)
    (output / "result.json").write_text(json.dumps({"status": "PASS", "cases": rows, "cli_overlap_controls": overlap,
        "prepared_refusal_controls": refusals,
        "scope": "Real Cargo, pinned nextest and native fixture processes; no daemon/product coverage."}, indent=2) + "\n")
    print("PASS: seven exact native cases; real Cargo-lock overlap; caller environment and single-worker limits; peer cancellation; inventory, output and prepared-state drift refused", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    run(parser.parse_args().output.resolve())
