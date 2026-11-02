"""Run the unchanged history case and preserve mock request bodies on exit."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess

parser = argparse.ArgumentParser()
parser.add_argument('--source', type=Path, required=True)
parser.add_argument('--output', type=Path, required=True)
args = parser.parse_args()
root, output = args.source.resolve(), args.output.resolve()
output.mkdir(parents=True, exist_ok=False)

def load(name, path):
    spec = importlib.util.spec_from_file_location(name, root / path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module

helper = load('wire_host', '.github/scripts/local_acceptance.py')
helper.ROOT = root
checks = load('wire_recipe', '.github/local-acceptance/checks.py')
layout = load('wire_cache', '.github/local-acceptance/cache_layout.py')
target = layout.resolve(Path(os.environ['GITHUB_WORKSPACE']), root / 'rust')['target']
source = root / 'rust/crates/rusty-sudocode-cli/tests/pty_background_tasks.rs'
original = source.read_bytes()
name = '_ci_background_history_wire_probe'
probe = source.with_name(name + '.rs')
assert not probe.exists()
needle = '    let (env, _release) = task_env("background-history-retention");'
text = original.decode('utf-8')
assert text.count(needle) == 1
capture = '''struct CaptureOnDrop<'a>(&'a TestEnv);
impl Drop for CaptureOnDrop<'_> {
    fn drop(&mut self) {
        if self.0.is_mock() {
            let path = std::env::var("SCODE_HISTORY_WIRE_CAPTURE").unwrap();
            let _ = fs::write(path, serde_json::to_vec(&self.0.captured_message_bodies()).unwrap());
        }
    }
}

'''
text = text.replace('fn task_env(', capture + 'fn task_env(')
instrumented = text.replace(needle, needle + '\n    let _capture = CaptureOnDrop(&env);').encode('utf-8')
env = {'SCODE_TEST_BACKEND': 'mock', 'CARGO_TARGET_DIR': target, 'CARGO_BUILD_JOBS': '1',
       'SCODE_HISTORY_WIRE_CAPTURE': str(output / 'requests.json')}
record = {'head': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=root, text=True).strip(),
          'source_sha256': hashlib.sha256(original).hexdigest(),
          'scope': 'Actual single history ConPTY case and mock requests, original180s turn deadline. Only capture-on-drop added; no default, product, assertion or acceptance signature change.'}
try:
    probe.write_bytes(instrumented)
    tool = checks.nextest_binary(output)
    record['command'] = helper.run_command([str(tool), 'nextest', 'run', *checks.nextest_options(output),
        '--locked', '-p', 'rusty-sudocode-cli', '--features', 'engine-acp/mailbox', '--test', name,
        '-E', 'test(=completed_task_history_prunes_in_the_open_session_without_losing_running_work)'],
        root / 'rust', output / 'case.log', env)
    record['status'] = 'PASS'
except BaseException as error:
    record.update(status='FAIL', error=repr(error))
    raise
finally:
    assert probe.read_bytes() == instrumented
    probe.unlink()
    assert source.read_bytes() == original
    (output / 'result.json').write_text(json.dumps(record, indent=2), encoding='utf-8')
    print(json.dumps(record), flush=True)
