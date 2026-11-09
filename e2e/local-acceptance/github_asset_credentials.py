"""Real HTTPS API/redirect and native-entry credential acceptance, without builds.

Only a dummy credential and isolated local TLS fixtures are used. A process-local
CONNECT proxy routes both named test hosts to the fixture; it refuses all other
destinations. No operator proxy, trust store or real GitHub credential is changed.
"""

import argparse
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
import os
from pathlib import Path
import select
import shutil
import socket
import ssl
import subprocess
import sys
import threading

PAYLOAD = b'published-asset-fixture\x00\xff\n'
TOKEN = 'GITHUB_ASSET_DUMMY_CREDENTIAL'


def driver(root, output, case, policy_ref=None):
    spec = importlib.util.spec_from_file_location('trusted_checks', root / '.github/local-acceptance/checks.py')
    checks = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(checks)
    if policy_ref:
        namespace = {'__name__': 'fixture_guard_tools'}
        exec(compile(checks.guard_tool_source(root, policy_ref), '<protected guard tool source>', 'exec'), namespace)
        download = namespace['github_asset_download']
    else:
        download = checks.github_asset_download
    download(f'fixture-{case}/artifact', 'v-fixture', 'asset.bin', output / f'{case}.bin')


def accept(args):
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    requests = []
    checks = []
    certificate = output / 'fixture-ca.pem'
    private_key = output / 'fixture-key.pem'
    configuration = output / 'openssl.conf'
    configuration.write_text('''[req]
distinguished_name=dn
x509_extensions=extensions
prompt=no
[dn]
CN=api.github.com
[extensions]
subjectAltName=DNS:api.github.com,DNS:storage.fixture.invalid
basicConstraints=critical,CA:TRUE
keyUsage=critical,keyCertSign,digitalSignature,keyEncipherment
extendedKeyUsage=serverAuth
subjectKeyIdentifier=hash
authorityKeyIdentifier=keyid:always
''')
    openssl = args.openssl or shutil.which('openssl')
    if not openssl and os.name == 'nt':
        git = shutil.which('git')
        bundled = Path(git).resolve().parents[1] / 'usr/bin/openssl.exe' if git else None
        if bundled is not None and bundled.is_file():
            openssl = bundled
    if not openssl:
        raise RuntimeError('Pass --openssl with the existing OpenSSL executable')
    subprocess.run([str(openssl), 'req', '-new', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
                    '-keyout', str(private_key), '-out', str(certificate), '-config', str(configuration)],
                   check=True, capture_output=True)

    class API(BaseHTTPRequestHandler):
        def do_GET(self):
            host = self.headers.get('Host')
            authenticated = self.headers.get('Authorization') == f'Bearer {TOKEN}'
            requests.append({'host': host, 'path': self.path,
                             'authorization_present': bool(self.headers.get('Authorization'))})
            if host == 'storage.fixture.invalid':
                self.send_response(200)
                self.send_header('Content-Length', str(len(PAYLOAD)))
                self.end_headers()
                self.wfile.write(PAYLOAD)
                return
            if host != 'api.github.com' or not authenticated:
                self.send_error(401, 'fixture credential required')
                return
            case = self.path.split('/')[2].removeprefix('fixture-')
            if case == 'denied':
                self.send_error(401, 'fixture refusal')
                return
            if '/releases/assets/' in self.path:
                self.send_response(302)
                self.send_header('Location', 'https://storage.fixture.invalid/artifact.bin')
                self.end_headers()
                return
            release = {'tag_name': 'wrong-tag' if case == 'badtag' else 'v-fixture',
                       'assets': [{'name': 'asset.bin', 'id': 42}]}
            if case == 'ambiguous':
                release['assets'].append({'name': 'asset.bin', 'id': 43})
            body = json.dumps(release).encode()
            self.send_response(200)
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args):
            pass

    api = ThreadingHTTPServer(('127.0.0.1', 0), API)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(certificate, private_key)
    api.socket = context.wrap_socket(api.socket, server_side=True)

    class Proxy(BaseHTTPRequestHandler):
        def do_CONNECT(self):
            if self.path not in {'api.github.com:443', 'storage.fixture.invalid:443'}:
                self.send_error(403, 'fixture refuses external destinations')
                return
            with socket.create_connection(('127.0.0.1', api.server_port)) as upstream:
                self.send_response(200)
                self.end_headers()
                while True:
                    readable, _, _ = select.select([self.connection, upstream], [], [], 5)
                    if not readable:
                        return
                    for source in readable:
                        data = source.recv(65536)
                        if not data:
                            return
                        (upstream if source is self.connection else self.connection).sendall(data)

        def log_message(self, *_args):
            pass

    proxy = ThreadingHTTPServer(('127.0.0.1', 0), Proxy)
    for server in (api, proxy):
        threading.Thread(target=server.serve_forever, daemon=True).start()
    env = os.environ.copy()
    env.update(GH_TOKEN=TOKEN, SSL_CERT_FILE=str(certificate),
               HTTPS_PROXY=f'http://127.0.0.1:{proxy.server_port}',
               HTTP_PROXY=f'http://127.0.0.1:{proxy.server_port}', NO_PROXY='',
               PYTHONUTF8='1', PYTHONIOENCODING='utf-8')
    env.pop('GITHUB_TOKEN', None)
    try:
        for case in ('valid', 'badtag', 'ambiguous', 'denied'):
            before = len(requests)
            command = [sys.executable, str(Path(__file__).resolve()), '--root', str(args.root),
                       '--output', str(output), '--download-case', case]
            if args.policy_ref:
                command += ['--policy-ref', args.policy_ref]
            result = subprocess.run(command, env=env, capture_output=True, timeout=40)
            (output / f'{case}.log').write_bytes(result.stdout + result.stderr)
            captured = requests[before:]
            if case == 'valid':
                assert result.returncode == 0, result.stderr.decode(errors='replace')
                assert (output / 'valid.bin').read_bytes() == PAYLOAD
                assert [item['host'] for item in captured] == ['api.github.com', 'api.github.com', 'storage.fixture.invalid'], captured
                assert [item['authorization_present'] for item in captured] == [True, True, False], captured
            else:
                assert result.returncode != 0 and not (output / f'{case}.bin').exists()
                assert len(captured) == 1 and captured[0]['host'] == 'api.github.com', captured
                expected = b'401' if case == 'denied' else b'Expected exactly one pinned'
                assert expected in result.stderr, result.stderr.decode(errors='replace')
            checks.append({'case': case, 'passed': True, 'requests': captured})

        result = subprocess.run([sys.executable, str(args.root / '.github/local-acceptance/cross_platform.py')],
                                env=env, cwd=args.root, capture_output=True, timeout=10)
        (output / 'native-token-refusal.log').write_bytes(result.stdout + result.stderr)
        assert result.returncode != 0
        assert b'Native candidate execution must not receive release-download credentials' in result.stderr
        checks.append({'case':'native_entry_refuses_download_token', 'passed':True})
    finally:
        for server in (proxy, api):
            server.shutdown()
            server.server_close()
        # This key was generated only for this test; never leave a private TLS
        # key in retained evidence. The resolved target is fixed under output.
        if private_key.resolve().parent != output:
            raise RuntimeError('Unexpected fixture-key cleanup target')
        private_key.unlink(missing_ok=True)
        (output / 'requests.json').write_text(json.dumps(requests, indent=2))
    receipt = {'passed':True, 'checks':checks, 'policy_ref':args.policy_ref,
               'payload_sha256':hashlib.sha256(PAYLOAD).hexdigest(),
               'scope':'Real local HTTPS/API/redirect and protected native-entry refusal; dummy key only; no real GitHub request, compilation, model call or CI acceptance claim',
               'source':{name:hashlib.sha256((args.root / name).read_bytes()).hexdigest() for name in (
                   '.github/local-acceptance/checks.py', '.github/local-acceptance/cross_platform.py',
                   '.github/local-acceptance/prepare_tools.py', '.github/workflows/local-acceptance.yml')}}
    (output / 'receipt.json').write_text(json.dumps(receipt, indent=2))
    print(json.dumps(receipt))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--openssl', type=Path)
    parser.add_argument('--download-case')
    parser.add_argument('--policy-ref')
    args = parser.parse_args()
    args.root = args.root.resolve()
    if args.download_case:
        driver(args.root, args.output, args.download_case, args.policy_ref)
    else:
        accept(args)
