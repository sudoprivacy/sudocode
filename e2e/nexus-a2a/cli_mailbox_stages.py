"""Run independent CLI journeys during the original mailbox lease waits."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time

from mailbox_faults import ROOT, run as run_faults


def execute_stage(arguments):
    """Use original tests and reserve one test worker for the serial CLI lane."""
    if arguments.stage == 'faults':
        os.environ['SCODE_ACCEPTANCE_TEST_THREADS'] = str(arguments.fault_workers)
        for target, source in [('NEXUS_A2A_TEST_ENDPOINT', 'ENDPOINT'),
                               ('NEXUS_A2A_TEST_CERT_DIR', 'CLIENT_BUNDLE')]:
            if source in os.environ:
                os.environ[target] = os.environ[source]
        run_faults(arguments.manifest, arguments.output)
    else:
        bash = shutil.which('bash')
        if not bash:
            raise RuntimeError('Bash is required for the original CLI daemon journeys')
        # One CLI case may create multiple actual scode processes; its original
        # assertions and deadlines stay unchanged while cases run serially.
        environment = {**os.environ, 'RUST_TEST_THREADS': '1'}
        result = subprocess.run([bash, str(arguments.cli_script)], cwd=ROOT, env=environment)
        if result.returncode:
            raise SystemExit(result.returncode)


def run(arguments):
    """Join both original lanes before the caller can pause its owned daemon."""
    workers = int(os.environ.get('SCODE_ACCEPTANCE_TEST_THREADS', '4'))
    if not 1 <= workers <= 4:
        raise RuntimeError('SCODE_ACCEPTANCE_TEST_THREADS must be between 1 and 4')
    output = arguments.output.parent / (arguments.output.name + '-stages')
    output.mkdir(parents=True, exist_ok=False)
    spec = importlib.util.spec_from_file_location('mailbox_cli_shared_supervisor',
                                                ROOT / '.github/local-acceptance/parallel_guard.py')
    supervisor = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(supervisor)
    common = [sys.executable, str(Path(__file__).resolve()), '--manifest', str(arguments.manifest),
              '--output', str(arguments.output), '--cli-script', str(arguments.cli_script)]
    data = {'version': 1, 'kind': 'native-policy', 'root': str(ROOT), 'branches': [
        {'name': name, 'commands': [{'command': [*common, '--stage', stage,
                                               '--fault-workers', str(max(1, workers - 1))],
                                    'log': str(output / (stage + '.log'))}]}
        for name, stage in [('protocol', 'faults'), ('native', 'cli')]]}
    configuration = output / 'commands.json'
    configuration.write_text(json.dumps(data, indent=2) + '\n')
    data = supervisor.read_configuration(configuration)
    print(f'A2A_STAGE_SCOPE: {workers} test workers total; '
          + ('serial CLI then faults (explicit one-worker cap).' if workers == 1
             else f'one serial CLI lane plus {workers - 1} independent fault workers.')
          + ' Original seven cases, lease clocks, deadlines and post-join daemon faults retained. '
          + f'Evidence: {output}', flush=True)
    try:
        if workers > 1:
            supervisor.run(configuration, data)
        else:
            run_serial(supervisor, configuration, data)
    finally:
        # Preserve the existing fault artifact layout, including failures before
        # the batch starts. Copy only after the shared supervisor joins cleanup.
        arguments.output.mkdir(parents=True, exist_ok=True)
        shutil.copytree(output, arguments.output / 'stages')


def run_serial(supervisor, configuration, data):
    """Respect an explicit one-worker limit using the same command executor."""
    outcome = {'version': 1, 'status': 'running', 'mode': 'serial-explicit-cap',
               'started_at': time.time(), 'branches': {}}
    try:
        for name in ['native', 'protocol']:
            supervisor.worker(data, name)
            outcome['branches'][name] = {'exit_code': 0, 'finished_at': time.time()}
        outcome['status'] = 'passed'
    except BaseException as error:
        outcome.update(status='failed', error=repr(error))
        raise
    finally:
        outcome['finished_at'] = time.time()
        configuration.with_suffix('.result.json').write_text(json.dumps(outcome, indent=2) + '\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--manifest', type=Path, default=ROOT / 'rust/Cargo.toml')
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--cli-script', type=Path, default=ROOT / 'e2e/nexus-a2a/run-cli.sh')
    parser.add_argument('--stage', choices=['cli', 'faults'], help=argparse.SUPPRESS)
    parser.add_argument('--fault-workers', type=int, default=1, help=argparse.SUPPRESS)
    arguments = parser.parse_args()
    arguments.manifest = arguments.manifest.resolve()
    arguments.output = arguments.output.resolve()
    arguments.cli_script = arguments.cli_script.resolve()
    if arguments.stage:
        execute_stage(arguments)
    else:
        run(arguments)
