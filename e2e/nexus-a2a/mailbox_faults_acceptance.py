"""Exercise batch selection and fail-fast through real Cargo/native processes."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import time

from mailbox_faults import CASES, ROOT


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
    (output / "result.json").write_text(json.dumps({"status": "PASS", "cases": rows,
        "scope": "Real Cargo, pinned nextest and native fixture processes; no daemon/product coverage."}, indent=2) + "\n")
    print("PASS: seven exact native cases, bounded overlap, first failure stops queued work, missing/non-ignored cases refused", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    run(parser.parse_args().output.resolve())
