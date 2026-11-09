"""Prepare pinned tools in a trusted step; candidate execution has no token."""

import importlib.util
import json
import os
from pathlib import Path


def prepare():
    names = os.environ['ACCEPTANCE_NATIVE_CHECKS'].split()
    if not names or any(not name.startswith('native-') for name in names):
        raise RuntimeError('Only native profiles may prepare these acceptance tools')
    if not (os.environ.get('GH_TOKEN') or os.environ.get('GITHUB_TOKEN')):
        raise RuntimeError('Trusted CI tool preparation requires its read-only GitHub token')
    # Resolve from this trusted checkout, never from the candidate or cwd.
    path = Path(__file__).resolve().with_name('checks.py')
    spec = importlib.util.spec_from_file_location('trusted_tool_checks', path)
    checks = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(checks)
    output = Path(os.environ['ACCEPTANCE_OUTPUT']) / 'tool-preparation'
    output.mkdir(parents=True, exist_ok=True)
    checks.nextest_binary(output)
    protoc = checks.cohost_protoc() if 'native-cohost' in names else None
    receipt = {
        'checks': names,
        'credential_scope': 'trusted tool preparation only; native executor refuses tokens',
        'nextest': json.loads((output / 'nextest-source.json').read_text()),
        'protoc': str(protoc) if protoc is not None else None,
    }
    (output / 'receipt.json').write_text(json.dumps(receipt, indent=2))
    print('TRUSTED_TOOL_PREPARATION=' + json.dumps(receipt), flush=True)


if __name__ == '__main__':
    prepare()
