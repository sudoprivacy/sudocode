"""Run independent real-daemon lease faults with the shared pinned executor."""

import argparse
import hashlib
import importlib.util
import json
import os
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


def digest(path):
    """Bind the bytes used by this invocation, including compiled test executables."""
    return hashlib.sha256(path.read_bytes()).hexdigest()


def source_state(metadata, output):
    """Refuse source drift without hashing Cargo outputs or this run's evidence."""
    workspace = Path(metadata['workspace_root']).resolve()
    target = Path(metadata['target_directory']).resolve()
    if workspace.is_relative_to(target):
        raise RuntimeError('Cargo target directory must stay outside source ancestry')
    excluded = [target, output.resolve(), output.with_name(output.name + '-stages').resolve()]

    def generated(path):
        return any(path == parent or path.is_relative_to(parent) for parent in excluded)

    environment = {key: value for key, value in os.environ.items() if not key.startswith('GIT_')}
    found = subprocess.run(['git', 'rev-parse', '--show-toplevel'], cwd=workspace,
                           env=environment, capture_output=True)
    repository = Path(os.fsdecode(found.stdout.strip())).resolve() if found.returncode == 0 else None
    tracked = (subprocess.run(['git', 'ls-files', '--error-unmatch', '--', str(workspace / 'Cargo.toml')],
                              cwd=repository, env=environment, capture_output=True).returncode == 0
               if repository else False)
    if tracked:
        listing = subprocess.check_output(['git', 'ls-files', '--cached', '--others', '--exclude-standard', '-z'],
                                          cwd=repository, env=environment)
        paths = [repository / os.fsdecode(name) for name in listing.split(b'\0') if name]
        base = repository
    else:
        base = workspace
        paths = []
        for directory, names, files in os.walk(workspace):
            parent = Path(directory)
            names[:] = [name for name in names if name != '.git' and not generated(parent / name)]
            paths.extend(parent / name for name in files)
    result = hashlib.sha256()
    count = 0
    for path in sorted(set(paths)):
        if generated(path):
            continue
        result.update(os.fsencode(path.relative_to(base).as_posix()) + b'\0')
        if path.is_symlink():
            if path.is_dir():
                raise RuntimeError(f'Cannot bind source directory symlink: {path}')
            result.update(b'link:' + os.fsencode(os.readlink(path)) + b'\0')
        if path.is_file():
            result.update(digest(path).encode())
        elif not path.exists():
            result.update(b'missing')
        else:
            raise RuntimeError(f'Cannot bind source entry: {path}')
        count += 1
    return {'root': str(base), 'files': count, 'sha256': result.hexdigest()}


def executor():
    """Load the same checksum-pinned nextest helpers for preparation and execution."""
    spec = importlib.util.spec_from_file_location('mailbox_fault_executor', ROOT / '.github/local-acceptance/checks.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def selection():
    """Keep the original seven exact names and ignored-only requirement together."""
    return ['-E', ' | '.join(f'test(={name})' for name in CASES), '--run-ignored', 'only']


def prepare(manifest, output, workers=None):
    """Finish Cargo work and prove the inventory before either independent lane starts."""
    start = time.monotonic()
    output.mkdir(parents=True, exist_ok=False)
    module = executor()
    executable = str(module.nextest_binary(output))
    options = module.nextest_options(output)
    if workers is not None:
        if not 1 <= workers <= 4:
            raise RuntimeError('Mailbox fault workers must stay between one and four')
        options[options.index('--test-threads') + 1] = str(workers)
    # These independent child processes share only a throwaway daemon owned
    # by the outer shell. A failed case must not wait for another real TTL.
    options[options.index("--max-fail") + 1] = "1:immediate"
    report = output / "junit.xml"
    with (output / "nextest.toml").open("a", encoding="utf-8") as stream:
        stream.write("\n[profile.default.junit]\npath = " + json.dumps(report.as_posix()) + "\n")
    cargo_metadata = output / 'cargo-metadata.json'
    binaries_metadata = output / 'binaries-metadata.json'
    cargo_metadata.write_bytes(subprocess.check_output(
        ['cargo', 'metadata', '--manifest-path', str(manifest), '--format-version', '1', '--locked'], cwd=manifest.parent))
    metadata = json.loads(cargo_metadata.read_bytes())
    original_source = source_state(metadata, output)
    binaries_metadata.write_bytes(subprocess.check_output(
        [executable, 'nextest', 'list', '--manifest-path', str(manifest), '--locked', '-p', 'runtime',
         '--test', 'mailbox_nexus_live', '--list-type', 'binaries-only', '--ignore-default-filter',
         '--message-format', 'json'], cwd=manifest.parent))
    reuse = ['--binaries-metadata', str(binaries_metadata), '--cargo-metadata', str(cargo_metadata)]
    listed = subprocess.check_output([executable, 'nextest', 'list', *reuse, *selection(),
                                      '--ignore-default-filter', '--message-format', 'json'], cwd=manifest.parent)
    (output / "inventory.json").write_bytes(listed)
    suites = list(json.loads(listed)["rust-suites"].values())
    selected = [(name, case) for suite in suites for name, case in suite["testcases"].items()
                if case["filter-match"]["status"] == "matches"]
    if (len(selected) != len(CASES) or {name for name, _ in selected} != set(CASES)
            or not all(case["ignored"] for _, case in selected)):
        raise RuntimeError(f"Expected exactly the seven ignored mailbox fault cases; see {output / 'inventory.json'}")
    binaries = {suite["binary-path"]: digest(Path(suite["binary-path"]))
                for suite in suites if suite["status"] == "listed"}
    if source_state(metadata, output) != original_source:
        raise RuntimeError('Mailbox source changed during preparation; retain evidence and use a fresh output')
    prepared = {'version': 1, 'manifest': str(manifest), 'cases': list(CASES), 'source': original_source,
                'program_sha256': digest(Path(__file__)), 'options': options, 'reuse': reuse,
                'binary_sha256': binaries, 'preparation_seconds': time.monotonic() - start,
                'files': {name: digest(output / name) for name in [
                    'cargo-metadata.json', 'binaries-metadata.json', 'inventory.json', 'nextest.toml', 'nextest-source.json']}}
    path = output / 'prepared.json'
    path.write_text(json.dumps(prepared, indent=2) + '\n')
    return digest(path)


def run(manifest, output, prepared_sha=None):
    """Execute the same pinned binaries without reacquiring Cargo's build lock."""
    if prepared_sha is None:
        prepared_sha = prepare(manifest, output)
    start = time.monotonic()
    if digest(output / 'prepared.json') != prepared_sha:
        raise RuntimeError('Mailbox preparation handoff changed; use a fresh output')
    prepared = json.loads((output / 'prepared.json').read_text())
    if prepared['manifest'] != str(manifest) or prepared['cases'] != list(CASES):
        raise RuntimeError('Mailbox preparation belongs to another manifest or case set')
    if prepared['options'][prepared['options'].index('--test-threads') + 1] != os.environ.get('SCODE_ACCEPTANCE_TEST_THREADS', '4'):
        raise RuntimeError('Mailbox preparation worker cap differs from this execution')

    def verify():
        if digest(output / 'prepared.json') != prepared_sha:
            raise RuntimeError('Mailbox preparation handoff changed; use a fresh output')
        if digest(Path(__file__)) != prepared['program_sha256']:
            raise RuntimeError('Mailbox executor source changed after preparation')
        if any(digest(output / name) != value for name, value in prepared['files'].items()):
            raise RuntimeError('Mailbox prepared metadata changed; use a fresh output')
        if any(digest(Path(path)) != value for path, value in prepared['binary_sha256'].items()):
            raise RuntimeError('Mailbox prepared binary changed; use a fresh output')
        if source_state(json.loads((output / 'cargo-metadata.json').read_bytes()), output) != prepared['source']:
            raise RuntimeError('Mailbox source changed after preparation; use a fresh output')

    verify()
    executable = str(executor().nextest_binary(output))
    command = [executable, 'nextest', 'run', *prepared['reuse'], *prepared['options'], *selection()]
    print(f"MAILBOX_FAULT_SCOPE: {len(CASES)} independent cases; original assertions and TTL waits. "
          f"Pinned executor, at most four workers, zero retries, first failure cancels this batch. Evidence: {output}", flush=True)
    result = subprocess.run(command, cwd=manifest.parent)
    record = {"command": command, "cases": list(CASES), "exit_code": result.returncode,
              "seconds": prepared['preparation_seconds'] + time.monotonic() - start,
              "binary_sha256": prepared['binary_sha256'], 'prepared_sha256': prepared_sha,
              'preparation_seconds': prepared['preparation_seconds'],
              "status": "executor-failed" if result.returncode else "pending-report-validation"}
    (output / "result.json").write_text(json.dumps(record, indent=2) + "\n")
    if result.returncode:
        raise SystemExit(result.returncode)
    verify()
    completed = list(ET.parse(output / 'junit.xml').iter("testcase"))
    if (len(completed) != len(CASES) or {case.attrib["name"] for case in completed} != set(CASES)
            or any(case.find(tag) is not None for case in completed for tag in ("failure", "error", "skipped"))):
        raise RuntimeError(f"Successful executor did not prove all seven cases passed; see {output / 'junit.xml'}")
    record["status"] = "PASS"
    record['execution_seconds'] = time.monotonic() - start
    record['seconds'] = prepared['preparation_seconds'] + record['execution_seconds']
    (output / "result.json").write_text(json.dumps(record, indent=2) + "\n")
    print(f"MAILBOX_FAULT_RESULT: all {len(CASES)} real-daemon fault cases passed; {record['seconds']:.3f}s", flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--manifest", type=Path, default=ROOT / "rust/Cargo.toml")
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    run(arguments.manifest.resolve(), arguments.output.resolve())
