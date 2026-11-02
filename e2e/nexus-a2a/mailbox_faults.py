"""Run independent real-daemon lease faults with the shared pinned executor."""

import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import time
import xml.etree.ElementTree as ET


ROOT = Path(__file__).resolve().parents[2]
CASES = (
    "live_reader_commit_refused_on_renew_error_then_reacquired_from_durable_offset",
    "live_stall_from_lease_lost_mid_batch_is_recorded_as_lease_lost",
    "live_refusal_under_a_held_lease_is_recorded_as_consumer_refused",
    "live_stall_from_a_lapsed_lease_is_recorded_as_lease_lost_with_its_age",
    "live_commit_refused_by_its_register_write_records_no_lease_loss",
    "live_retry_diagnostics_distinguish_commit_lapse_from_register_io",
    "live_receiver_seat_trusts_only_fresh_renewals",
)


def run(manifest, output):
    """Require every named ignored case before running its unchanged assertions."""
    output.mkdir(parents=True, exist_ok=False)
    spec = importlib.util.spec_from_file_location("mailbox_fault_executor", ROOT / ".github/local-acceptance/checks.py")
    executor = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(executor)
    executable = str(executor.nextest_binary(output))
    options = executor.nextest_options(output)
    # These independent child processes share only a throwaway daemon owned
    # by the outer shell. A failed case must not wait for another real TTL.
    options[options.index("--max-fail") + 1] = "1:immediate"
    report = output / "junit.xml"
    with (output / "nextest.toml").open("a", encoding="utf-8") as stream:
        stream.write("\n[profile.default.junit]\npath = " + json.dumps(report.as_posix()) + "\n")
    selection = ["--manifest-path", str(manifest), "--locked", "-p", "runtime",
                 "--test", "mailbox_nexus_live", "-E", " | ".join(f"test(={name})" for name in CASES),
                 "--run-ignored", "only"]
    start = time.monotonic()
    listed = subprocess.check_output([executable, "nextest", "list", *selection,
                                      "--ignore-default-filter", "--message-format", "json"], cwd=manifest.parent)
    (output / "inventory.json").write_bytes(listed)
    suites = list(json.loads(listed)["rust-suites"].values())
    selected = [(name, case) for suite in suites for name, case in suite["testcases"].items()
                if case["filter-match"]["status"] == "matches"]
    if (len(selected) != len(CASES) or {name for name, _ in selected} != set(CASES)
            or not all(case["ignored"] for _, case in selected)):
        raise RuntimeError(f"Expected exactly the seven ignored mailbox fault cases; see {output / 'inventory.json'}")
    binaries = {suite["binary-path"]: hashlib.sha256(Path(suite["binary-path"]).read_bytes()).hexdigest()
                for suite in suites if suite["status"] == "listed"}
    command = [executable, "nextest", "run", *options, *selection]
    print(f"MAILBOX_FAULT_SCOPE: {len(CASES)} independent cases; original assertions and TTL waits. "
          f"Pinned executor, at most four workers, zero retries, first failure cancels this batch. Evidence: {output}", flush=True)
    result = subprocess.run(command, cwd=manifest.parent)
    record = {"command": command, "cases": list(CASES), "exit_code": result.returncode,
              "seconds": time.monotonic() - start,
              "binary_sha256": binaries,
              "status": "executor-failed" if result.returncode else "pending-report-validation"}
    (output / "result.json").write_text(json.dumps(record, indent=2) + "\n")
    if result.returncode:
        raise SystemExit(result.returncode)
    if any(hashlib.sha256(Path(path).read_bytes()).hexdigest() != digest for path, digest in binaries.items()):
        raise RuntimeError("Mailbox fault executable changed during the batch; retain evidence and rerun a fresh output")
    completed = list(ET.parse(report).iter("testcase"))
    if (len(completed) != len(CASES) or {case.attrib["name"] for case in completed} != set(CASES)
            or any(case.find(tag) is not None for case in completed for tag in ("failure", "error", "skipped"))):
        raise RuntimeError(f"Successful executor did not prove all seven cases passed; see {report}")
    record["status"] = "PASS"
    (output / "result.json").write_text(json.dumps(record, indent=2) + "\n")
    print(f"MAILBOX_FAULT_RESULT: all {len(CASES)} real-daemon fault cases passed; {record['seconds']:.3f}s", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=ROOT / "rust/Cargo.toml")
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    run(arguments.manifest.resolve(), arguments.output.resolve())
