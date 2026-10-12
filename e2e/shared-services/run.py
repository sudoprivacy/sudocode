"""Real CLI -> managed Nexus -> signed Vault, using an isolated service root."""
import argparse
import base64
from concurrent.futures import Future, ThreadPoolExecutor
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
import uuid


class Rpc:
    def __init__(self, binary, root):
        env = os.environ | {'SCODE_SERVICE_ROOT': str(root),
                            'SUDO_CODE_CONFIG_HOME': str(root / 'config'),
                            'SCODE_CHECK_UPDATES': '0', 'RUST_LOG': 'info'}
        self.child = subprocess.Popen([str(binary), 'service', 'stdio'], env=env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            text=True, encoding='utf-8',
            creationflags=subprocess.CREATE_NO_WINDOW if os.name == 'nt' else 0)
        self.lock = threading.Lock()
        self.pending = {}
        self.sequence = 0
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        try:
            for line in self.child.stdout:
                response = json.loads(line)
                with self.lock:
                    future = self.pending.pop(response['id'], None)
                if future is None:
                    raise AssertionError('Unmatched service response')
                if 'error' in response:
                    future.set_exception(RuntimeError(response['error']['message']))
                else:
                    future.set_result(response['result'])
        finally:
            with self.lock:
                for future in self.pending.values():
                    future.set_exception(RuntimeError('Service connection closed'))
                self.pending.clear()

    def submit(self, method, params=None):
        with self.lock:
            self.sequence += 1
            future = Future()
            self.pending[self.sequence] = future
            self.child.stdin.write(json.dumps({'id': self.sequence, 'method': method,
                                               'params': params or {}}) + '\n')
            self.child.stdin.flush()
        return future

    def request(self, method, params=None, timeout=60):
        return self.submit(method, params).result(timeout)

    def close(self):
        self.child.stdin.close()
        self.child.wait(timeout=45)


def run(binary, root):
    a, b = Rpc(binary, root), Rpc(binary, root)
    session = None
    try:
        assert not a.request('service.checkInstalled')['installed']
        a.request('service.install', timeout=600)
        with ThreadPoolExecutor(2) as workers:
            first = workers.submit(a.request, 'service.start')
            second = workers.submit(b.request, 'service.start')
            first, second = first.result(), second.result()
        assert first['nexusPid'] == second['nexusPid'] and first['instance'] == second['instance']
        assert first['zone_id'], 'Readiness must name the serving zone'
        print('PASS: concurrent clients share one authenticated ready service', flush=True)
        plugins = a.request('service.checkInstalled')['plugins']
        assert len(plugins) >= 2, 'Vault and local connector must be present'
        log = (root / 'run/runtime.log').read_text(encoding='utf-8')
        for plugin in plugins:
            assert plugin in log, f'Expected plugin was not loaded: {plugin}'
        print('PASS: selected signed plugins are loaded from the verified assembly', flush=True)

        discovery = json.loads((root / 'run/service.json').read_text())
        probe = urllib.request.Request(discovery['endpoint'],
            data=json.dumps({'id': 99, 'method': 'vault.list', 'params': {}}).encode(),
            headers={'Content-Type': 'application/json', 'Authorization': 'Bearer invalid'})
        try:
            urllib.request.urlopen(probe, timeout=5)
            raise AssertionError('Unauthenticated access succeeded')
        except urllib.error.HTTPError as error:
            assert error.code == 401
        print('PASS: broker rejects a caller without its connection credential', flush=True)

        namespace = 'acceptance:' + uuid.uuid4().hex
        secret = 'synthetic-value-中文-' + uuid.uuid4().hex
        a.request('vault.put', {'namespace': namespace, 'key': 'sample', 'value': secret})
        a.request('vault.put', {'namespace': namespace, 'key': 'sample', 'value': secret + '-rotated'})
        assert b.request('vault.get', {'namespace': namespace, 'key': 'sample'}) == secret + '-rotated'
        assert a.request('vault.get', {'namespace': namespace, 'key': 'sample', 'version': 1}) == secret
        assert len(a.request('vault.versions', {'namespace': namespace, 'key': 'sample'})) == 2
        assert secret not in json.dumps(a.request('vault.list', {'namespace': namespace}))
        a.request('vault.delete', {'namespace': namespace, 'key': 'sample'})
        assert b.request('vault.get', {'namespace': namespace, 'key': 'sample'}) is None
        b.request('vault.restore', {'namespace': namespace, 'key': 'sample'})
        print('PASS: encrypted Vault versions, deletion, restoration and metadata', flush=True)

        a.request('files.mkdir', {'path': '/acceptance/body'})
        data = ('durable-body-中文-' + uuid.uuid4().hex).encode() + b'x' * (2 * 1024 * 1024)
        a.request('files.write', {'path': '/acceptance/body/content', 'data': base64.b64encode(data).decode()})
        a.request('files.link', {'path': '/acceptance/body/alias', 'target': '/acceptance/body/content'})
        assert base64.b64decode(b.request('files.read', {'path': '/acceptance/body/alias'})) == data
        assert a.request('files.exists', {'path': '/acceptance/body/content'})
        assert not a.request('files.exists', {'path': '/acceptance/body/absent'})
        print('PASS: typed files and links resolve identical durable bytes', flush=True)

        started = a.request('sessions.call', {'method': 'managed_agent.start_session_v1', 'params': {
            'agent_id': 'service-acceptance', 'spawn_spec': {'cmd': str(binary), 'args': ['acp'],
            'env': os.environ | {'SCODE_CHECK_UPDATES': '0', 'SUDO_CODE_CONFIG_HOME': str(root / 'config')},
            'cwd': str(root)}}})
        session = started['session_id']
        # Obtain the endpoint from the control-plane result, keeping address SSOT.
        endpoint = started.get('session_endpoint')
        if endpoint is None:
            raise AssertionError('Managed runtime did not return its session endpoint')
        waiting = a.submit('sessions.read', {'path': endpoint['transcript'], 'offset': '0', 'blocking': True, 'timeoutMs': 1000})
        begin = time.monotonic()
        a.request('vault.list', {'namespace': namespace}, timeout=5)
        assert time.monotonic() - begin < 1, 'A waiting session read blocked Vault'
        waiting.result(5)
        for method in ['service.shutdown', 'service.restart', 'service.removeInstallation']:
            try:
                b.request(method)
                raise AssertionError(f'{method} interrupted a managed runtime')
            except RuntimeError as error:
                assert 'Agent runtimes' in str(error)
        a.request('sessions.call', {'method': 'managed_agent.cancel_v1', 'params': {'session_id': session, 'mode': 'session'}})
        session = None
        print('PASS: session waits preserve concurrent capabilities and active runtimes block shutdown', flush=True)

        a.close()
        a = None
        assert b.request('service.status')['running']
        b.request('service.shutdown')
        b.close()
        b = Rpc(binary, root)
        b.request('service.start')
        assert b.request('vault.get', {'namespace': namespace, 'key': 'sample'}) == secret + '-rotated'
        assert base64.b64decode(b.request('files.read', {'path': '/acceptance/body/alias'})) == data
        # Installation removal is restricted to assemblies, retaining the data root.
        b.request('service.removeInstallation')
        assert not b.request('service.checkInstalled')['installed']
        assert not b.request('service.status')['running']
        b.request('service.shutdown')
        assert not b.request('service.checkInstalled')['installed'], 'Shutdown recreated an installation'
        b.request('service.install', timeout=600)
        b.request('service.start')
        assert b.request('vault.get', {'namespace': namespace, 'key': 'sample'}) == secret + '-rotated'
        print('PASS: frontend closure, service restart and installation repair preserve data', flush=True)
    finally:
        if b:
            if session:
                b.request('sessions.call', {'method': 'managed_agent.cancel_v1', 'params': {'session_id': session, 'mode': 'session'}})
            b.request('service.shutdown')
        if a:
            a.close()
        if b:
            b.close()
        deadline = time.monotonic() + 15
        while (root / 'run/service.json').exists():
            if time.monotonic() > deadline:
                raise AssertionError('Service shutdown did not finish')
            time.sleep(0.1)


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--scode', required=True, type=Path)
    options = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='scode-services-') as temporary:
        run(options.scode.resolve(), Path(temporary))
