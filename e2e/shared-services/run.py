"""Real CLI -> managed Nexus -> signed Vault, using an isolated service root."""
import argparse
import base64
from concurrent.futures import Future, ThreadPoolExecutor
from contextlib import contextmanager
import json
import os
import shutil
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
                            'SCODE_CHECK_UPDATES': '0', 'RUST_LOG': 'info',
                            'NEXUS_IDENTITY_DIR': str(root / 'unrelated-identity')}
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


@contextmanager
def retiring_service_lock(root):
    """Model a retiring owner with the same real OS lifetime lock as the daemon."""
    with (root / 'run/service.lock').open('a+b') as lock:
        lock.seek(0)
        if os.name == 'nt':
            import msvcrt
            msvcrt.locking(lock.fileno(), msvcrt.LK_NBLCK, 1)
        else:
            import fcntl
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        try:
            yield
        finally:
            lock.seek(0)
            if os.name == 'nt':
                msvcrt.locking(lock.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(lock, fcntl.LOCK_UN)


def run(binary, root):
    unrelated = root / 'unrelated-identity' / 'identity.json'
    unrelated.parent.mkdir()
    unrelated_body = json.dumps({'schema_version': 2, 'peers': [],
        'zones': [{'zone_id': 'foreign-fixture-zone', 'members': [], 'as_role': 'voter'}]})
    unrelated.write_text(unrelated_body, encoding='utf-8')
    a, b = Rpc(binary, root), Rpc(binary, root)
    session = None
    try:
        assert not a.request('service.checkInstalled')['installed']
        a.request('service.install', timeout=600)
        with ThreadPoolExecutor(2) as workers:
            # A shutdown response can precede release of the daemon's lock. A
            # losing launcher is not a future owner: both real frontend start
            # requests must recover when that retiring owner releases the lock.
            with retiring_service_lock(root):
                first = workers.submit(a.request, 'service.start')
                second = workers.submit(b.request, 'service.start')
                time.sleep(4)
                assert not first.done() and not second.done(), 'Startup ignored the lifetime owner'
            first, second = first.result(), second.result()
        assert first['nexusPid'] == second['nexusPid'] and first['instance'] == second['instance']
        assert first['zone_id'], 'Readiness must name the serving zone'
        node_id = (root / 'data/.node_id').read_bytes()
        identity = json.loads((root / 'identity/identity.json').read_text())
        assert all(zone['zone_id'] != 'foreign-fixture-zone' for zone in identity['zones'])
        assert unrelated.read_text(encoding='utf-8') == unrelated_body
        print('PASS: concurrent clients share one authenticated ready service', flush=True)
        print('PASS: concurrent start requests survive a retiring lifetime-lock owner', flush=True)
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
        other_root = root / 'independent-service'
        other_root.mkdir()
        shutil.copytree(root / 'runtimes', other_root / 'runtimes')
        independent = Rpc(binary, other_root)
        try:
            isolated = independent.request('service.start')
            assert isolated['nexusPid'] != first['nexusPid'] and isolated['instance'] != first['instance']
            assert independent.request('vault.get', {'namespace': namespace, 'key': 'sample'}) is None
            assert (root / 'data/.node_id').read_bytes() != (other_root / 'data/.node_id').read_bytes()
            assert (other_root / 'identity/identity.json').is_file()
            print('PASS: independent managed roots isolate identity, node ID and Vault content', flush=True)
        finally:
            independent.request('service.shutdown')
            independent.close()
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
        restarted = b.request('service.start')
        assert restarted['instance'] != first['instance'], 'Restart must replace the stopped supervisor'
        assert (root / 'data/.node_id').read_bytes() == node_id, 'Restart changed the durable node identity'
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
        assert (root / 'data/.node_id').read_bytes() == node_id, 'Installation repair changed the durable node identity'
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
    parser.add_argument('--artifacts', type=Path,
                        help='Copy service runtime logs here when acceptance fails')
    options = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix='scode-services-') as temporary:
        root = Path(temporary)
        try:
            run(options.scode.resolve(), root)
        except BaseException:
            if options.artifacts:
                for log in root.rglob('runtime*.log'):
                    destination = options.artifacts / log.relative_to(root)
                    destination.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy2(log, destination)
                print(f'Service failure logs: {options.artifacts}', flush=True)
            raise
