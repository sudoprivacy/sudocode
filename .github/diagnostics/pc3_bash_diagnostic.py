"""Observe original Mac PTY cases; this diagnostic never waives acceptance."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import time

import psutil


def git(source, *arguments):
    return subprocess.check_output(['git', *arguments], cwd=source).decode().strip()


def run_observed(command, source, output, name, *, compiling=False):
    started = time.monotonic()
    captured = set()
    denied = set()
    proof = {'command': command, 'compiling': compiling, 'samples': [], 'permissionRejections': []}
    with (output / (name + '.log')).open('wb') as log:
        child = subprocess.Popen(command, cwd=source / 'rust', stdout=log, stderr=log)
        while child.poll() is None:
            sample = {'seconds': round(time.monotonic() - started, 2), 'processes': []}
            try:
                descendants = psutil.Process(child.pid).children(recursive=True)
            except psutil.NoSuchProcess:
                descendants = []
            for process in descendants:
                if process.pid in denied:
                    continue
                try:
                    executable = process.name()
                    if compiling or executable in ('rustc', 'cc', 'ld', 'clang'):
                        process.nice(10)
                    arguments = process.cmdline()
                    sample['processes'].append({
                        'pid': process.pid, 'parent': process.ppid(), 'name': executable,
                        'arguments': arguments, 'nice': process.nice(),
                        'cpu': list(process.cpu_times()[:2]),
                        'status': process.status(), 'rss': process.memory_info().rss,
                    })
                    # The original failure waited 30 seconds for shell output.
                    # Take one bounded, read-only stack sample while that exact
                    # PTY still owns its scode child, before the test cleans up.
                    if not compiling and executable == 'scode' and process.pid not in captured:
                        parent = process.parent()
                        if parent and parent.pid not in denied and any('bang_runs_shell_command_without_model_turn' in arg for arg in parent.cmdline()):
                            age = time.time() - process.create_time()
                            if age >= 10:
                                captured.add(process.pid)
                                result = subprocess.run(
                                    ['/usr/bin/sample', str(process.pid), '1', '1', '-file',
                                     str(output / f'{name}-scode-{process.pid}.sample.txt')],
                                    capture_output=True, text=True, timeout=8,
                                )
                                (output / f'{name}-sample-{process.pid}.json').write_text(json.dumps({
                                    'exit': result.returncode, 'stdout': result.stdout, 'stderr': result.stderr,
                                }, indent=2))
                                if result.returncode:
                                    raise RuntimeError('Stack observation failed; preserve its exact result without another attachment method')
                except psutil.NoSuchProcess:
                    pass
                except psutil.AccessDenied as error:
                    # Preserve the refusal and never read this PID again. Do
                    # not query another tool, change identity or elevate.
                    denied.add(process.pid)
                    if error.pid is not None:
                        denied.add(error.pid)
                    proof['permissionRejections'].append({
                        'pid': process.pid, 'reason': str(error),
                        'cause': str(error.__cause__) if error.__cause__ else None,
                        'seconds': round(time.monotonic() - started, 2),
                    })
            proof['samples'].append(sample)
            time.sleep(.5)
        proof.update(exit=child.wait(), seconds=round(time.monotonic() - started, 2))
    (output / (name + '.json')).write_text(json.dumps(proof, indent=2))
    return proof['exit'] if not proof['permissionRejections'] else 2


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--source', type=Path, required=True)
    parser.add_argument('--expected-source', required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--scope', choices=['group', 'workspace'], default='group')
    parser.add_argument('--rounds', type=int, choices=range(1, 4), default=3)
    args = parser.parse_args()
    source, output = args.source.resolve(), args.output.resolve()
    assert sys.platform == 'darwin'
    assert git(source, 'rev-parse', 'HEAD') == args.expected_source
    assert git(source, 'status', '--porcelain') == ''
    output.mkdir(parents=True, exist_ok=False)
    assert os.environ['SCODE_TEST_BACKEND'] == 'mock'
    spec = importlib.util.spec_from_file_location('source_checks', source / '.github/local-acceptance/checks.py')
    checks = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(checks)
    nextest = str(checks.nextest_binary(output))
    arguments = ['--locked', '-p', 'rusty-sudocode-cli', '--test', 'acceptance_cli_05', '--features', 'engine-acp/mailbox']
    selection = {'mode': 'original-group'}
    if args.scope == 'workspace':
        metadata = json.loads(subprocess.check_output(
            ['cargo', 'metadata', '--no-deps', '--locked', '--format-version=1'], cwd=source / 'rust'))
        targets, selection = checks.grouped_workspace_test_targets(source, metadata)
        arguments = ['--workspace', '--locked', '--features', 'engine-acp/mailbox',
                     *checks.workspace_integration_test_targets(targets)]
    proof = {'source': args.expected_source, 'mockOnly': True, 'acceptanceWaiver': False,
             'originalDeadlinesAndAssertions': True, 'workers': 4, 'rounds': [],
             'scope': args.scope, 'selection': selection}
    try:
        options = (checks.workspace_nextest_options(output, bool(targets),
                   hosted_source_ready=checks.supports_hosted_eight_workers(source))
                   if args.scope == 'workspace' else checks.nextest_options(output))
        assert options[options.index('--test-threads') + 1] == '4'
        code = run_observed(['cargo', 'test', '--no-run', *arguments], source, output, 'compile', compiling=True)
        assert code == 0, 'Original native test targets must build'
        for number in range(1, args.rounds + 1):
            code = run_observed([nextest, 'nextest', 'run', *options, *arguments], source, output, f'round-{number}')
            proof['rounds'].append({'round': number, 'exit': code})
            if code:
                break
        proof['passed'] = bool(proof['rounds']) and all(item['exit'] == 0 for item in proof['rounds'])
    finally:
        proof['sourceUnchanged'] = git(source, 'rev-parse', 'HEAD') == args.expected_source and git(source, 'status', '--porcelain') == ''
        (output / 'receipt.json').write_text(json.dumps(proof, indent=2))
    print(json.dumps(proof), flush=True)
    return 0 if proof['passed'] and proof['sourceUnchanged'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
