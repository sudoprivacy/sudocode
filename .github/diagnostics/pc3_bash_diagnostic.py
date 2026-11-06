"""Observe original Mac PTY cases; this diagnostic never waives acceptance."""

import argparse
import hashlib
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


def permission_cause(error):
    """psutil's C wrapper can surface an OS PermissionError as SystemError."""
    seen = set()
    while error is not None and id(error) not in seen:
        seen.add(id(error))
        if isinstance(error, (psutil.AccessDenied, PermissionError)):
            return error
        error = error.__cause__ or error.__context__
    return None


def normal_child_priority():
    # Hosted job launchers can start at nice -10. Lower only this owned child
    # to normal; actual tests must not inherit a compiler's nice 10.
    current = os.getpriority(os.PRIO_PROCESS, 0)
    if current < 0:
        os.nice(-current)
    if os.getpriority(os.PRIO_PROCESS, 0) != 0:
        raise RuntimeError('Launch this diagnostic at normal test priority')


def add_pipe_window_failure_diagnostics(source):
    """Add only failure output to the disposable candidate, never a product change."""
    relative = 'rust/crates/rusty-sudocode-cli/tests/pty_tool_concurrency.rs'
    path = source / relative
    original = subprocess.check_output(['git', 'show', 'HEAD:' + relative], cwd=source)
    assert path.read_bytes() == original and not path.is_symlink()
    needle = '''            Err(error) => panic!(
                "reader did not start for {name}: {error}\\n{}",
                sess.render(common::screen_contents)
            ),'''
    replacement = '''            Err(error) => panic!(
                "reader did not start for {name}: {error}\\n{}\\nCaptured mock tool requests: {:#?}",
                sess.render(common::screen_contents),
                env.captured_message_bodies()
            ),'''
    text = original.decode()
    assert text.count(needle) == 1
    path.write_text(text.replace(needle, replacement), encoding='utf-8')
    patch = subprocess.check_output(['git', 'diff', 'HEAD', '--binary'], cwd=source)
    assert git(source, 'diff', '--name-only').splitlines() == [relative]
    return patch


def run_observed(command, source, output, name, *, compiling=False):
    started = time.monotonic()
    captured = set()
    denied = set()
    proof = {'command': command, 'compiling': compiling, 'samples': [], 'permissionRejections': []}
    with (output / (name + '.log')).open('wb') as log:
        child = subprocess.Popen(command, cwd=source / 'rust', stdout=log, stderr=log,
                                 preexec_fn=normal_child_priority)
        while child.poll() is None:
            sample = {'seconds': round(time.monotonic() - started, 2), 'processes': []}
            try:
                descendants = psutil.Process(child.pid).children(recursive=True)
            except psutil.NoSuchProcess:
                descendants = []
            for process in descendants:
                if process.pid in denied:
                    continue
                observed_pid = process.pid
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
                        parent_arguments = []
                        if parent and parent.pid not in denied:
                            observed_pid = parent.pid
                            parent_arguments = parent.cmdline()
                            observed_pid = process.pid
                        if any('bang_runs_shell_command_without_model_turn' in arg for arg in parent_arguments):
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
                except (psutil.AccessDenied, PermissionError, SystemError) as error:
                    cause = permission_cause(error)
                    if cause is None:
                        raise
                    # Preserve the refusal and never read this PID again. Do
                    # not query another tool, change identity or elevate.
                    denied.add(process.pid)
                    denied.add(observed_pid)
                    if getattr(cause, 'pid', None) is not None:
                        denied.add(cause.pid)
                    proof['permissionRejections'].append({
                        'pid': observed_pid, 'reason': str(error),
                        'cause': str(cause),
                        'seconds': round(time.monotonic() - started, 2),
                    })
                    # Retain the refusal even if a later observation fails.
                    (output / (name + '.json')).write_text(json.dumps(proof, indent=2))
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
    parser.add_argument('--scope', choices=['group', 'workspace', 'pipe-window'], default='group')
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
    diagnostic_patch = b''
    test_filter = []
    if args.scope == 'workspace':
        metadata = json.loads(subprocess.check_output(
            ['cargo', 'metadata', '--no-deps', '--locked', '--format-version=1'], cwd=source / 'rust'))
        targets, selection = checks.grouped_workspace_test_targets(source, metadata)
        arguments = ['--workspace', '--locked', '--features', 'engine-acp/mailbox',
                     *checks.workspace_integration_test_targets(targets)]
    elif args.scope == 'pipe-window':
        diagnostic_patch = add_pipe_window_failure_diagnostics(source)
        (output / 'failure-diagnostics.patch').write_bytes(diagnostic_patch)
        arguments = ['--locked', '-p', 'rusty-sudocode-cli', '--test', 'acceptance_cli_06',
                     '--features', 'engine-acp/mailbox']
        test_filter = ['-E', 'test(=pty_tool_concurrency::concurrent_shell_capture_is_private_during_the_pipe_cloexec_window)']
        selection = {'mode': 'original-case-with-added-failure-output',
                     'diagnosticPatchSha256': hashlib.sha256(diagnostic_patch).hexdigest(),
                     'runtimeSourceChanged': False, 'assertionsDeadlinesGeometryChanged': False}
    proof = {'source': args.expected_source, 'mockOnly': True, 'acceptanceWaiver': False,
             'originalDeadlinesAndAssertions': True, 'workers': 4, 'rounds': [],
             'scope': args.scope, 'selection': selection}
    try:
        options = (checks.workspace_nextest_options(output, bool(targets))
                   if args.scope == 'workspace' else checks.nextest_options(output))
        assert options[options.index('--test-threads') + 1] == '4'
        code = run_observed(['cargo', 'test', '--no-run', *arguments], source, output, 'compile', compiling=True)
        assert code == 0, 'Original native test targets must build'
        for number in range(1, args.rounds + 1):
            code = run_observed([nextest, 'nextest', 'run', *options, *arguments, *test_filter], source, output, f'round-{number}')
            proof['rounds'].append({'round': number, 'exit': code})
            if code:
                break
        proof['passed'] = bool(proof['rounds']) and all(item['exit'] == 0 for item in proof['rounds'])
    finally:
        same_head = git(source, 'rev-parse', 'HEAD') == args.expected_source
        status = git(source, 'status', '--porcelain')
        proof['sourceUnchanged'] = same_head and status == ''
        expected_status = ('M rust/crates/rusty-sudocode-cli/tests/pty_tool_concurrency.rs'
                           if diagnostic_patch else '')
        proof['diagnosticInputsUnchanged'] = same_head and status == expected_status and subprocess.check_output(
            ['git', 'diff', 'HEAD', '--binary'], cwd=source) == diagnostic_patch
        (output / 'receipt.json').write_text(json.dumps(proof, indent=2))
    print(json.dumps(proof), flush=True)
    return 0 if proof['passed'] and proof['diagnosticInputsUnchanged'] else 1


if __name__ == '__main__':
    raise SystemExit(main())
