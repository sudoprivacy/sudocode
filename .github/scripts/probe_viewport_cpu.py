"""Non-landing diagnostic. Original failures remain failures, never acceptance."""
import datetime
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import traceback

ROOT = Path.cwd()
OUT = Path(os.environ['RUNNER_TEMP']) / 'viewport-cpu-qualification'
OUT.mkdir()
shutil.copy2(__file__, OUT / 'controller.py')
expected_path = ROOT / '.github/scripts/viewport_probe_expected.json'
expected = json.loads(expected_path.read_text())
shutil.copy2(expected_path, OUT / expected_path.name)
ORIGINAL = '0f8c80681d91cab9d93ada10c7e20a2bc123a85d'
SOURCE = '9213035f0028bfe67bffb12c2971788c14755821'
receipt = {'started_at': datetime.datetime.now(datetime.timezone.utc).isoformat(),
           'not_candidate_acceptance': True, 'never_merge_this_branch': True,
           'original_ci_checkout': ORIGINAL, 'original_pr_head': SOURCE,
           'original_ci_images': {k: expected[k] for k in ['binary_sha256', 'driver_sha256']},
           'stages': {}, 'status': 'running'}


def save():
    (OUT / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')


def capture(*cmd):
    return subprocess.check_output(cmd, cwd=ROOT, text=True).strip()


def run(cmd, log, cwd=ROOT, timeout=1800):
    print(json.dumps({'command': cmd, 'cwd': str(cwd), 'log': str(log)}), flush=True)
    with log.open('w') as output:
        result = subprocess.run(cmd, cwd=cwd, stdout=output,
                                stderr=subprocess.STDOUT, timeout=timeout)
    if result.returncode:
        print(log.read_text(errors='replace')[-6000:], flush=True)
        raise RuntimeError(f'command failed: {cmd[0]} exit={result.returncode}')


def validate_harness():
    actual = {name: hashlib.sha256((ROOT / name).read_bytes()).hexdigest()
              for name in expected['harness_sha256']}
    assert actual == expected['harness_sha256'], 'measurement source/policy changed'
    return actual


def build(label, revision):
    subprocess.run(['git', 'checkout', '--detach', revision], cwd=ROOT, check=True)
    stage = {'checkout': capture('git', 'rev-parse', 'HEAD'),
             'tree': capture('git', 'rev-parse', 'HEAD^{tree}'),
             'harness_sha256': validate_harness()}
    receipt['stages'][label] = stage
    save()
    run(['cargo', 'build', '--release', '--locked', '-p', 'rusty-sudocode-cli',
         '--bin', 'scode'], OUT / f'{label}-cli-build.log', ROOT / 'rust')
    records = OUT / f'{label}-driver-build.jsonl'
    with records.open('w') as output, (OUT / f'{label}-driver-build.stderr').open('w') as error:
        subprocess.run(['cargo', 'test', '--release', '--locked', '-p', 'rusty-sudocode-cli',
                        '--test', 'pty_prompt_viewport', '--no-run',
                        '--message-format=json-render-diagnostics'],
                       cwd=ROOT / 'rust', stdout=output, stderr=error,
                       timeout=1800, check=True)
    artifacts = [json.loads(line) for line in records.read_text().splitlines()]
    drivers = [row['executable'] for row in artifacts
               if row.get('reason') == 'compiler-artifact'
               and row.get('target', {}).get('name') == 'pty_prompt_viewport'
               and row.get('executable')]
    assert len(drivers) == 1, drivers
    images = OUT / 'images' / label
    images.mkdir(parents=True)
    # Tests can rebuild the CLI with dev-dependency feature unification. Seal
    # after the driver build, matching the original CI command sequence.
    cli = images / 'scode'
    driver = images / 'pty_prompt_viewport'
    shutil.copy2(ROOT / 'rust/target/release/scode', cli)
    shutil.copy2(drivers[0], driver)
    stage.update(binary_sha256=hashlib.sha256(cli.read_bytes()).hexdigest(),
                 driver_sha256=hashlib.sha256(driver.read_bytes()).hexdigest())
    save()
    return cli, driver


def measure(label, binary, original_driver):
    destination = OUT / label
    cmd = [sys.executable, 'scripts/benchmark_prompt_viewport.py', '--driver',
           str(original_driver), '--binary', str(binary), '--output', str(destination)]
    with (OUT / f'{label}.log').open('w') as output:
        result = subprocess.run(cmd, cwd=ROOT, stdout=output,
                                stderr=subprocess.STDOUT, timeout=600)
    report = json.loads((destination / 'report.json').read_text())
    assert report['status'] in ['passed', 'failed', 'noisy', 'invalid']
    receipt['stages'][label] = {'returncode': result.returncode,
                               'status': report['status'], 'failures': report.get('failures', []),
                               'binary_sha256': report['binary_sha256'],
                               'driver_sha256': report['driver_sha256'],
                               'metrics': report.get('metrics', [])}
    save()
    return report


try:
    assert sys.platform == 'linux'
    receipt['rustc'] = capture('rustc', '--version')
    assert 'rustc 1.99.1 ' in receipt['rustc']
    candidate_ref = os.environ['VIEWPORT_CANDIDATE_REF']
    subprocess.run(['git', 'check-ref-format', '--branch', candidate_ref], check=True)
    receipt['candidate_ref'] = candidate_ref
    save()
    old_cli, old_driver = build('original', ORIGINAL)
    assert receipt['stages']['original']['tree'] == capture('git', 'rev-parse', SOURCE + '^{tree}')
    receipt['original_ci_images_reconstructed_exactly'] = {
        key: receipt['stages']['original'][key] == expected[key]
        for key in ['binary_sha256', 'driver_sha256']}
    run(['npm', 'ci', '--prefix', 'e2e/terminal-resize'], OUT / 'node-install.log')
    before = measure('original-measurement', old_cli, old_driver)
    # Resolve once after the original control. The immutable resolved commit,
    # never a moving branch, is compiled and reported below.
    subprocess.run(['git', 'fetch', '--no-tags', 'origin', f'refs/heads/{candidate_ref}'],
                   cwd=ROOT, check=True)
    candidate = capture('git', 'rev-parse', 'FETCH_HEAD')
    subprocess.run(['git', 'merge-base', '--is-ancestor', SOURCE, candidate],
                   cwd=ROOT, check=True)
    receipt.update(candidate_checkout=candidate,
                   source_changes=capture('git', 'diff', '--name-only', SOURCE, candidate).splitlines())
    save()
    new_cli, _new_driver = build('candidate', candidate)
    assert receipt['stages']['candidate']['tree'] != receipt['stages']['original']['tree'], 'candidate source unchanged; no fix to validate'
    after = measure('candidate-measurement', new_cli, old_driver)
    assert before['driver_sha256'] == after['driver_sha256']
    receipt['controlled_fix_qualified'] = (
        before['status'] == 'failed'
        and any(row['metric'] == 'cpu_ms' and row['result'] == 'regressed'
                for row in before.get('metrics', []))
        and after['status'] == 'passed')
    receipt['status'] = 'collected'
except Exception:
    receipt['status'] = 'diagnostic-error'
    receipt['error'] = traceback.format_exc()
    raise
finally:
    receipt['finished_at'] = datetime.datetime.now(datetime.timezone.utc).isoformat()
    save()
    print(json.dumps(receipt, indent=2), flush=True)
