"""Real Windows bootstrap readiness diagnostic; never candidate acceptance."""
import argparse
import datetime
import hashlib
import json
from pathlib import Path
import shutil
import subprocess
import sys
import time

p = argparse.ArgumentParser()
p.add_argument('--grpcurl', required=True)
a = p.parse_args()
ROOT = Path.cwd()
OUT = ROOT / 'box-bootstrap-probe'
OUT.mkdir(exist_ok=False)
BASE = '302b6ee6306cebbf347a55fc7ee2cd09f4098de5'
allowed = {'.github/workflows/rust-ci.yml', '.github/scripts/probe_box_bootstrap.py',
           'e2e/shared-services/existing_box.py'}
changed = set(subprocess.check_output(['git', 'diff', '--name-only', BASE, 'HEAD'], text=True).splitlines())
assert changed <= allowed, changed
assert not subprocess.check_output(['git', 'diff', '--name-only', BASE, 'HEAD', '--', 'rust/'])
record = {'head': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
          'base': BASE, 'scope': 'Diagnostic only; unchanged product source, real private native Node and actual CLI',
          'toolchain': subprocess.check_output(['rustc', '--version'], text=True).strip(),
          'original_first_failure_run': 40797800371, 'original_first_failure_job': 123198378509,
          'original_windows_image_not_uploaded': True, 'unchanged_retry_requested': False,
          'functional_deadlines_unchanged': True, 'changed_paths': sorted(changed),
          'started_at': datetime.datetime.now(datetime.timezone.utc).isoformat()}

def save():
    (OUT / 'receipt.json').write_text(json.dumps(record, indent=2) + '\n', encoding='utf-8')

save()
with (OUT / 'compile.log').open('xb') as stream:
    r = subprocess.run(['cargo', 'build', '--locked', '-p', 'rusty-sudocode-cli'],
                       cwd=ROOT / 'rust', stdout=stream, stderr=subprocess.STDOUT)
record['compile_exit_code'] = r.returncode
save()
if r.returncode:
    sys.exit(r.returncode)
original = ROOT / 'rust/target/debug/scode.exe'
sealed = OUT / 'bin/scode.exe'
sealed.parent.mkdir()
before = hashlib.sha256(original.read_bytes()).hexdigest()
shutil.copy2(original, sealed)
assert before == hashlib.sha256(sealed.read_bytes()).hexdigest() == hashlib.sha256(original.read_bytes()).hexdigest()
record['scode_sha256'] = before
record['script_sha256'] = hashlib.sha256((ROOT / 'e2e/shared-services/existing_box.py').read_bytes()).hexdigest()
command = [sys.executable, str(ROOT / 'e2e/shared-services/existing_box.py'),
           '--scode', str(sealed), '--grpcurl', a.grpcurl, '--artifacts', str(OUT / 'existing-box')]
record['command'] = command
save()
start = time.monotonic()
with (OUT / 'existing-box.log').open('xb') as stream:
    r = subprocess.run(command, cwd=ROOT, stdout=stream, stderr=subprocess.STDOUT)
record.update(exit_code=r.returncode, seconds=round(time.monotonic()-start, 2),
              binary_unchanged=hashlib.sha256(sealed.read_bytes()).hexdigest() == before)
text = (OUT / 'existing-box.log').read_text(encoding='utf-8', errors='replace')
record['observations'] = [json.loads(line.split('BOOTSTRAP_OBSERVATION ', 1)[1])
                          for line in text.splitlines() if 'BOOTSTRAP_OBSERVATION ' in line]
record['pass_steps'] = [line for line in text.splitlines() if line.startswith('PASS:')]
receipt = OUT / 'existing-box/receipt.json'
if receipt.exists():
    product = json.loads(receipt.read_text(encoding='utf-8'))
    record['native_receipt'] = product
    assert product['daemonSha256'] == '1e355bee0a50bcdb4b449d7841ce6e958b7d44ab740e50a4e4f905b9e8538b55'
save()
print(json.dumps(record), flush=True)
sys.exit(r.returncode)
