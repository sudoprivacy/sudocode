"""One changed-observer control with the exact previously failing Windows images."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time

ROOT = Path.cwd()
OUT = ROOT / 'error-observation-probe'
OUT.mkdir(exist_ok=False)
PREVIOUS = ROOT / 'previous-probe'
previous = json.loads((PREVIOUS / 'receipt.json').read_text(encoding='utf-8'))
assert previous['raw_render_mismatch_proved'] and previous['images_unchanged']
assert previous['head'] == 'd44d07394d59ece1afec62facfa24e56fe33a6eb'
BASE = '63f569ce885c6bce0939b8f956e4683f2690f242'
allowed = {'.github/workflows/rust-ci.yml', '.github/scripts/probe_error_observation.py',
           'rust/crates/rusty-sudocode-cli/tests/pty_cache_prefix.rs'}
changed = set(subprocess.check_output(['git', 'diff', '--name-only', BASE, 'HEAD'], text=True).splitlines())
assert changed <= allowed, changed
# No Rust source changed after sealing the negative's test executable.
assert not subprocess.check_output(['git', 'diff', '--name-only', previous['head'], 'HEAD', 'rust/'])
for path, digest in previous['images'].items():
    assert hashlib.sha256((PREVIOUS / path).read_bytes()).hexdigest() == digest
record = {'head': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
          'scope': 'Causal diagnostic, not PR acceptance; unchanged CLI AND test images, mode raw to screen',
          'negative_run': 40795813479, 'negative_artifact': 12864480776,
          'negative_validated': True, 'toolchain': subprocess.check_output(['rustc', '--version'], text=True).strip(),
          'conditions': [], 'images': previous['images']}

def save():
    (OUT / 'receipt.json').write_text(json.dumps(record, indent=2) + '\n', encoding='utf-8')

save()
env = dict(os.environ, SCODE_TEST_BACKEND='mock',
           SCODE_TEST_BIN=str(PREVIOUS / 'bin/scode-before-observation-fix.exe'),
           SCODE_ERROR_OBSERVATION_PROBE_MODE='screen')
for key in ['RUST_TEST_THREADS', 'SCODE_ERROR_OBSERVATION_PROBE_COLUMNS']:
    env.pop(key, None)
target_libdir = subprocess.check_output(['rustc', '--print', 'target-libdir'], text=True).strip()
env['PATH'] = os.pathsep.join([target_libdir, env.get('PATH', '')])
for suite in ['pty_bracketed_paste', 'pty_cache_prefix', 'pty_compaction_safety']:
    command = [str(PREVIOUS / f'bin/{suite}-diagnostic.exe'), '--nocapture']
    log = OUT / f'{suite}-screen.log'
    start = time.monotonic()
    with log.open('xb') as stream:
        result = subprocess.run(command, cwd=ROOT / 'rust', env=env,
                                stdout=stream, stderr=subprocess.STDOUT)
    observations = []
    for line in log.read_text(encoding='utf-8', errors='replace').splitlines():
        if 'ERROR_PROBE_OBSERVATION ' in line:
            observations.append(json.loads(line.split('ERROR_PROBE_OBSERVATION ', 1)[1]))
    row = {'suite': suite, 'mode': 'screen', 'geometry': 'original default24x80; no resize',
           'threads': 'original default', 'exit_code': result.returncode,
           'seconds': round(time.monotonic() - start, 2), 'command': command,
           'log': log.name, 'observations': observations}
    record['conditions'].append(row)
    save()
    print(json.dumps({k: row[k] for k in ['suite', 'mode', 'exit_code', 'seconds']}), flush=True)
    if result.returncode != 0:
        record['unclassified_positive_failure'] = True
        save()
        sys.exit(2)
record['positive_control_passed'] = True
record['images_unchanged'] = all(hashlib.sha256((PREVIOUS / path).read_bytes()).hexdigest() == digest
                                 for path, digest in previous['images'].items())
assert record['images_unchanged']
save()
