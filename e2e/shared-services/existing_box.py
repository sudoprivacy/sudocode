"""Real isolated mTLS Node -> explicit ownership -> actual CLI crash recovery.

Never accepts an operator data directory or client record. The initial Node is
created under the test directory. Stops are deliberate process termination,
not a claim of graceful Node shutdown or multi-machine HA.
"""
import argparse
import base64
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import socket
import subprocess
import tempfile
import time
import urllib.request

from run import Rpc, client_environment


def sha(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def wait_until(action, timeout=30):
    deadline = time.monotonic() + timeout
    last_error = None
    while True:
        try:
            value = action()
            if value:
                return value
        except (OSError, subprocess.SubprocessError, ValueError, KeyError) as error:
            last_error = error
        if time.monotonic() >= deadline:
            raise AssertionError(f'Native existing-Box acceptance did not reach its predicate: {last_error}')
        time.sleep(.1)


def run(args, root):
    if args.daemon is None:
        installation = root / 'installation'
        installation.mkdir()
        installer = Rpc(args.scode, installation)
        try:
            installer.request('service.install', timeout=600)
        finally:
            installer.close()
        binaries = list(installation.glob('runtimes/*/bin/nexusd-cluster' + ('.exe' if os.name == 'nt' else '')))
        assert len(binaries) == 1, binaries
        args.daemon = binaries[0].resolve(strict=True)
    data, identity, services = [root / name for name in ['data', 'identity', 'services']]
    for directory in [data, identity, services]:
        directory.mkdir()
    env = client_environment(services)
    # Native bootstrap gets a private OS environment, not the operator's Node
    # bootstrap, auth, model, or discovery configuration.
    keep = {'PATH', 'SYSTEMROOT', 'WINDIR', 'COMSPEC', 'PATHEXT', 'TEMP', 'TMP',
        'SYSTEMDRIVE', 'NUMBER_OF_PROCESSORS', 'PROCESSOR_ARCHITECTURE',
        'HOME', 'USERPROFILE', 'SCODE_SERVICE_ROOT', 'SCODE_GLOBAL_CONFIG_DIR',
        'SCODE_PROJECT_CONFIG_DIR', 'SUDO_CODE_CONFIG_HOME', 'SCODE_CHECK_UPDATES'}
    env = {key: value for key, value in env.items() if key.upper() in keep}
    # Reproduce an existing SudoWork service root. Recovery must use a durable
    # explicitly selected owner, not replace that data or need an env override.
    frontend = root / 'frontend-services'
    sentinel = frontend / 'data/original-sudowork-state'
    sentinel.parent.mkdir(parents=True)
    sentinel.write_bytes(b'keep-existing-frontend-state')
    env['SCODE_SERVICE_ROOT'] = str(frontend)
    with socket.socket() as listener:
        listener.bind(('127.0.0.1', 0))
        port = listener.getsockname()[1]
    endpoint = f'127.0.0.1:{port}'
    bind_addr = f'{args.bind_host}:{port}'
    native_env = env | {'NEXUS_DATA_DIR': str(data), 'NEXUS_IDENTITY_DIR': str(identity),
        'NEXUS_ADVERTISE_ADDR': endpoint, 'NEXUS_CLUSTER_INIT': 'sharedzone',
        'NEXUS_CLUSTER_INIT_MOUNTS': '/agents=sharedzone', 'RUST_LOG': 'info'}
    daemon_log = (root / 'bootstrap.log').open('wb')
    bootstrap = subprocess.Popen([str(args.daemon), '--bind-addr', bind_addr],
        env=native_env, stdout=daemon_log, stderr=subprocess.STDOUT)
    children = []
    receipt = {'daemonSha256': sha(args.daemon), 'scodeSha256': sha(args.scode),
        'liveFleetTouched': False, 'haAccepted': False, 'domainJoinAccepted': False,
        'bindAddr': bind_addr, 'clientEndpoint': endpoint}

    def grpc(method, payload, raft=False, zones=False):
        proto = Path(__file__).with_name('cluster-info.proto') if raft else (
            Path(__file__).with_name('zone-status.proto') if zones else
            Path(__file__).resolve().parents[2] / 'rust/crates/nexus-vfs-client/proto/nexus/grpc/vfs/vfs.proto')
        command = [str(args.grpcurl), '-max-time', '3', '-cacert', str(data / 'tls/ca.pem'),
            '-cert', str(data / 'tls/node.pem'), '-key', str(data / 'tls/node-key.pem'),
            '-servername', 'nexus-node', '-import-path', str(proto.parent), '-proto', proto.name, '-d', json.dumps(payload),
            endpoint, ('nexus.raft.ZoneApiService/' if raft else 'nexus.grpc.vfs.ZoneRuntimeService/' if zones
                       else 'nexus.grpc.vfs.NexusVFSService/') + method]
        result = subprocess.run(command, capture_output=True, timeout=5)
        if result.returncode:
            with (root / 'grpc-errors.log').open('ab') as output:
                output.write(result.stderr)
            result.check_returncode()
        return json.loads(result.stdout)

    def peer_messaging_resident():
        # Readiness loads the zone that holds /agents before it is published.
        # Without that, a restart reports ready while the zone is still
        # HOSTED_NOT_RESIDENT, and the first client read pays the whole load.
        presence = grpc('ZoneStatus', {'zone_id': 'sharedzone'}, zones=True).get('presence')
        assert presence == 'RESIDENT', presence

    def membership():
        return {zone: grpc('GetClusterInfo', {'zone_id': zone}, True)
                for zone in ['root', 'sharedzone', '__control__']}

    def cli(arguments, expected=True, environment=None):
        result = subprocess.run([str(args.scode), *arguments], env=environment or env,
            cwd=root, stdin=subprocess.DEVNULL, capture_output=True, timeout=55)
        with (root / 'cli.log').open('ab') as output:
            output.write(result.stdout + result.stderr)
        assert (result.returncode == 0) == expected, (result.stdout + result.stderr).decode(errors='replace')
        return json.loads(result.stdout) if expected else result

    def setup(environment=None, owner_root=None):
        arguments = ['setup', '--mode', 'shared', '--output-format', 'json']
        if owner_root:
            arguments += ['--owner-root', str(owner_root)]
        return cli(arguments, environment=environment)

    def broker(method):
        discovery = json.loads((services / 'run/service.json').read_bytes())
        request = urllib.request.Request(discovery['endpoint'], data=json.dumps({
            'id': 1, 'method': method, 'params': {}}).encode(), headers={
                'Content-Type': 'application/json', 'Authorization': 'Bearer ' + discovery['token']})
        with urllib.request.urlopen(request, timeout=5) as response:
            return json.load(response)

    try:
        wait_until(lambda: data.joinpath('tls/node.pem').is_file())
        before = wait_until(membership)
        assert all(item['nodeId'] == before['root']['nodeId'] for item in before.values())
        marker = base64.b64encode('original durable Box body 中文\x00'.encode()).decode()
        grpc('Write', {'path': '/agents/lifecycle-proof', 'content': marker})
        assert grpc('Read', {'path': '/agents/lifecycle-proof'})['content'] == marker
        profile = {'schemaVersion': 1, 'boxEndpoint': endpoint, 'binary': str(args.daemon),
            'binarySha256': sha(args.daemon), 'dataDir': str(data), 'identityDir': str(identity),
            'nodeId': str(int.from_bytes((data / '.node_id').read_bytes(), 'big')), 'caSha256': sha(data / 'tls/ca.pem'),
            'certificateSha256': sha(data / 'tls/node.pem'), 'localEndpoint': endpoint,
            'advertiseAddr': endpoint, 'bindAddr': bind_addr, 'hostname': 'localhost',
            'serverName': 'nexus-node'}
        config = root / 'owner.json'
        config.write_text(json.dumps(profile))
        record = Path(env['HOME']) / '.nexus/run/node.json'
        record.parent.mkdir(parents=True)
        original_record = json.dumps({'version': 1, 'endpoint': endpoint, 'agents_dir': str(data / 'agents')}).encode()
        record.write_bytes(original_record)
        for field, bad_value in [('binarySha256', '0' * 64), ('nodeId', '1'),
                ('localEndpoint', f'192.0.2.1:{port}'), ('args', ['--force'])]:
            bad = root / 'bad.json'
            bad.write_text(json.dumps(profile | {field: bad_value}))
            cli(['service', 'register-local-box', '--config', str(bad), '--owner-root', str(services)], expected=False)
            assert not (services / 'run/local-box.json').exists()
        cli(['service', 'register-local-box', '--config', str(config)], expected=False)
        assert sentinel.read_bytes() == b'keep-existing-frontend-state'
        registered = cli(['service', 'register-local-box', '--config', str(config), '--owner-root', str(services)])
        assert registered['registered'] and registered['started'] is False
        assert not (services / 'run/service.json').exists()
        cli(['service', 'register-local-box', '--config', str(config), '--owner-root', str(services)], expected=False)
        external = setup(owner_root=services)['service']
        preference = json.loads(Path(env['SCODE_GLOBAL_CONFIG_DIR']).joinpath('runtime-profile.json').read_bytes())
        assert preference['ownerRoot'] == str(services)
        assert external['managed'] is False and bootstrap.poll() is None
        assert not (services / 'run/service.json').exists()
        print('PASS: explicit pins/refusals; registration and occupied-Box reuse start no replacement', flush=True)

        bootstrap.terminate()
        bootstrap.wait(timeout=15)
        daemon_log.close()
        persisted = data / 'sharedzone/raft/raft.redb'
        missing = persisted.with_suffix('.test-missing')
        persisted.rename(missing)
        try:
            cli(['setup', '--mode', 'shared', '--output-format', 'json'], expected=False)
            assert not (services / 'run/service.json').exists()
            assert not persisted.exists(), 'Recovery initialized missing original zone state'
        finally:
            missing.rename(persisted)
        record.write_text(json.dumps({'version': 1, 'endpoint': f'localhost:{port}',
            'agents_dir': str(data / 'agents')}))
        try:
            cli(['setup', '--mode', 'shared', '--output-format', 'json'], expected=False)
            assert not (services / 'run/service.json').exists()
        finally:
            record.write_bytes(original_record)
        print('PASS: missing original zone state and a different configured Box refuse before launch', flush=True)
        # Poisoned launch env must never replace the original state or request
        # a new cluster. Only the owner profile's native resume inputs apply.
        poisoned = env | {'NEXUS_DATA_DIR': str(root / 'wrong-data'),
            'NEXUS_NO_TLS': 'true', 'NEXUS_INSECURE_NO_AUTH': 'true',
            'NEXUS_CLUSTER_INIT': 'wrong-zone'}
        if os.name == 'nt':
            poisoned.pop('NEXUS_NO_TLS')
            poisoned['nexus_no_tls'] = 'true'
        with ThreadPoolExecutor(max_workers=3) as pool:
            results = list(pool.map(lambda _: setup(poisoned), range(3)))
        discovery = json.loads((services / 'run/service.json').read_bytes())
        children.append(discovery)
        assert all(result['service']['persistedNodeId'] == profile['nodeId'] for result in results)
        assert all(result['service']['managed'] is True for result in results), results
        assert all(result['service']['instance'] == discovery['instance'] for result in results), results
        assert broker('service.status')['result']['managed'] is True
        for method in ['files.read', 'vault.list', 'sessions.call', 'service.shutdown', 'service.restart']:
            assert 'error' in broker(method), method
        assert not (root / 'wrong-data').exists()
        assert not (services / 'runtimes').exists(), 'Recovery downloaded another native version'
        peer_messaging_resident()
        assert grpc('Read', {'path': '/agents/lifecycle-proof'})['content'] == marker
        after = membership()
        for zone, old in before.items():
            assert after[zone]['nodeId'] == old['nodeId']
            assert after[zone]['config'] == old['config'], (zone, old, after[zone])
        assert setup()['service']['instance'] == discovery['instance']
        assert record.read_bytes() == original_record
        assert sentinel.read_bytes() == b'keep-existing-frontend-state'
        print('PASS: concurrent real CLI recovery preserves Node/CA/body/all zone memberships and frontend closure', flush=True)
        stop = ['service', 'stop-local-box', '--node-id', profile['nodeId'],
            '--binary-sha256', profile['binarySha256']]
        cli(stop, expected=False)
        wrong = stop.copy()
        wrong[3] = '1'
        cli(wrong + ['--acknowledge-shared-outage'], expected=False)
        assert broker('service.status')['result']['instance'] == discovery['instance']
        old_instance = discovery['instance']
        stopped = cli(stop + ['--acknowledge-shared-outage'])
        assert stopped['stopped'] and stopped['gracefulRaftDrain'] is False
        children.clear()
        assert not (services / 'run/service.json').exists()
        restarted = setup()['service']
        discovery = json.loads((services / 'run/service.json').read_bytes())
        children.append(discovery)
        assert restarted['instance'] != old_instance
        assert restarted['persistedNodeId'] == profile['nodeId']
        peer_messaging_resident()
        assert grpc('Read', {'path': '/agents/lifecycle-proof'})['content'] == marker
        print('PASS: acknowledged exact-target operator termination and recovery preserve state', flush=True)
        receipt.update(nodeId=profile['nodeId'], before=before, after=after,
            ownerPid=discovery['supervisor_pid'], nexusPid=discovery['nexus_pid'], status='PASS')
    finally:
        if bootstrap.poll() is None:
            bootstrap.terminate()
            bootstrap.wait(timeout=15)
        daemon_log.close()
        # Only PIDs returned by this private owner are targeted. Terminating its
        # child exercises the native crash path; the supervisor observes exit.
        for child in children:
            cli(['service', 'stop-local-box', '--node-id', profile['nodeId'],
                '--binary-sha256', profile['binarySha256'], '--acknowledge-shared-outage'])
        if children:
            wait_until(lambda: not (services / 'run/service.json').exists(), 15)
        (root / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    for name in ['scode', 'grpcurl', 'artifacts']:
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--daemon', type=Path, help='Otherwise install the signed/pinned native package through scode')
    parser.add_argument('--bind-host', choices=['127.0.0.1', '0.0.0.0'], default='127.0.0.1',
        help='Exercise a loopback or wildcard Node listener through the same loopback client endpoint')
    options = parser.parse_args()
    for name in ['scode', 'grpcurl']:
        setattr(options, name, getattr(options, name).resolve(strict=True))
    if options.daemon:
        options.daemon = options.daemon.resolve(strict=True)
    options.artifacts.mkdir(parents=True, exist_ok=True)
    # Keep sanitized logs/receipt, never temporary TLS private keys or databases.
    with tempfile.TemporaryDirectory(prefix='scode-existing-box-') as temporary:
        root = Path(temporary)
        try:
            run(options, root)
        finally:
            import shutil
            for name in ['bootstrap.log', 'cli.log', 'grpc-errors.log', 'receipt.json']:
                if (root / name).is_file():
                    shutil.copy2(root / name, options.artifacts / name)
