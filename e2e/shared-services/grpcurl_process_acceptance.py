"""Exercise existing-Box polling with real private synthetic client processes.

This is a process-result regression, not a grpcurl runtime-crash reproduction.
The installed pinned client and original mTLS journeys remain separate checks.
"""

import argparse
import base64
import json
from pathlib import Path
import subprocess
import sys
import time

import existing_box as acceptance
from existing_box import VfsRpcError


CHILD = '''import json,sys
from pathlib import Path
root=Path(sys.argv[1])
counter=root/'calls.txt'
count=int(counter.read_text()) if counter.exists() else 0
plan=json.loads((root/'plan.json').read_text(encoding='utf-8'))
row=plan[min(count,len(plan)-1)]
counter.write_text(str(count+1),encoding='ascii')
sys.stdout.buffer.write(row['stdout'].encode('utf-8'))
sys.stderr.buffer.write(bytes.fromhex(row['stderr_hex']))
sys.exit(row['exit'])
'''


def legacy_response(result, method, payload, root):
    """The original response body, retained verbatim as the failure control."""
    if result.returncode:
        with (root / 'grpc-errors.log').open('ab') as output:
            output.write(f'{method} {payload.get("path", payload.get("zone_id", ""))}:\n'.encode())
            output.write(result.stderr)
        result.check_returncode()
    response = json.loads(result.stdout)
    if response.get('isError'):
        error = VfsRpcError(method, response)
        with (root / 'grpc-errors.log').open('ab') as output:
            output.write((json.dumps({'method': method, 'response': error.response, 'error': error.detail}) + '\n').encode())
        raise error
    return response


def row(code, stdout=b'{}', stderr=b''):
    return {'exit': code, 'stdout': stdout.decode('utf-8'), 'stderr_hex': stderr.hex()}


SUCCESS = row(0, b'{"ready":true}')
FATAL = b'ERROR:\n  Code: NotFound\n  Message: private synthetic zone\nfatal error: fault\n[signal 0xc0000005]\n\xff\n'


def action(root, plan, parser):
    root.mkdir()
    (root / 'plan.json').write_text(json.dumps(plan), encoding='utf-8')
    child = root / 'client.py'
    child.write_text(CHILD, encoding='utf-8')

    def call():
        result = subprocess.run([sys.executable, str(child), str(root)],
                                capture_output=True, timeout=5)
        with (root / 'grpc-calls.jsonl').open('a', encoding='utf-8') as output:
            output.write(json.dumps({'at': time.time(), 'method': 'GetClusterInfo',
                                     'path': None, 'zone': 'private-synthetic-zone',
                                     'exit': result.returncode,
                                     'error': result.stderr.decode('utf-8', errors='replace')}) + '\n')
        return parser(result, 'GetClusterInfo', {'zone_id': 'private-synthetic-zone'}, root)

    return call


def fatal_contract(root, parser, failure):
    call = action(root, [failure, SUCCESS], parser)
    try:
        acceptance.wait_until(call)
    except acceptance.GrpcurlProcessError:
        pass
    else:
        raise AssertionError('A failed client process was retried to a later successful response')
    assert (root / 'calls.txt').read_text() == '1', 'Fail-fast must issue exactly one process'
    calls = [json.loads(line) for line in (root / 'grpc-calls.jsonl').read_text(encoding='utf-8').splitlines()]
    assert len(calls) == 1 and calls[0]['exit'] == 2
    stderr = bytes.fromhex(failure['stderr_hex'])
    assert calls[0]['error'] == stderr.decode('utf-8', errors='replace')
    assert (root / 'grpc-errors.log').read_bytes() == b'GetClusterInfo private-synthetic-zone:\n' + stderr


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    output = args.output.resolve()
    source = Path(__file__).resolve().parents[2]
    if output == source or source in output.parents:
        raise RuntimeError('Keep process-result fixtures outside the source checkout')
    output.mkdir(parents=True, exist_ok=False)
    cases = []
    for name, failure in [
            ('normal-status-prefix-then-runtime-fatal', row(2, stderr=FATAL)),
            ('runtime-fatal-without-status', row(2, stderr=b'fatal error: fault\n')),
            ('successful-json-followed-by-process-failure', row(2, b'{"ready":true}', FATAL)),
            ('flag-process-failure', row(2, stderr=b'flag provided but not defined: -synthetic\n'))]:
        fatal_contract(output / name, acceptance.checked_grpcurl_response, failure)
        cases.append({'name': name, 'processes': 1, 'passed': True})

    mutant = output / 'original-retry-control'
    try:
        fatal_contract(mutant, legacy_response, row(2, stderr=FATAL))
    except AssertionError as error:
        assert str(error) == 'A failed client process was retried to a later successful response'
    else:
        raise AssertionError('The unchanged original response logic did not fail the same fatal-process contract')
    assert (mutant / 'calls.txt').read_text() == '2'
    cases.append({'name': 'original-retry-control', 'processes': 2, 'unsafeOriginalBehaviorObserved': True})

    vfs = {'isError': True, 'errorPayload': base64.b64encode(b'{"code":-32007}').decode()}
    for name, failure in [
            ('transport-exit-1', row(1, stderr=b'connection refused\n')),
            ('rpc-cancelled-65', row(65, stderr=b'ERROR:\n  Code: Canceled\n')),
            ('rpc-unknown-is-66', row(66, stderr=b'ERROR:\n  Code: Unknown\n')),
            ('rpc-not-found-69', row(69, stderr=b'ERROR:\n  Code: NotFound\n')),
            ('rpc-unavailable-78', row(78, stderr=b'ERROR:\n  Code: Unavailable\n')),
            ('malformed-json', row(0, b'{malformed')),
            ('typed-vfs-error', row(0, json.dumps(vfs).encode()))]:
        root = output / name
        result = acceptance.wait_until(action(root, [failure, SUCCESS], acceptance.checked_grpcurl_response))
        assert result == {'ready': True}
        assert (root / 'calls.txt').read_text() == '2', 'Original transient handling must remain'
        calls = [json.loads(line) for line in (root / 'grpc-calls.jsonl').read_text(encoding='utf-8').splitlines()]
        assert [call['exit'] for call in calls] == [failure['exit'], 0]
        cases.append({'name': name, 'processes': 2, 'passed': True})

    root = output / 'first-success'
    assert acceptance.wait_until(action(root, [SUCCESS], acceptance.checked_grpcurl_response)) == {'ready': True}
    assert (root / 'calls.txt').read_text() == '1'
    cases.append({'name': 'first-success', 'processes': 1, 'passed': True})
    (output / 'result.json').write_text(json.dumps({
        'cases': cases,
        'originalWaitUntilUsed': True,
        'realSyntheticClientProcesses': True,
        'realGrpcurlCrashReproducedOrFixed': False,
        'installedClientOrNativeJourneyQualification': False,
    }, indent=2) + '\n', encoding='utf-8')
    print('PASS: process-exit2 stops with complete diagnostics; original RPC, transport, JSON and VFS retries remain')


if __name__ == '__main__':
    main()
