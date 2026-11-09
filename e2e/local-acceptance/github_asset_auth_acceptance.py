"""Real HTTPS/redirect and native subprocess controls for scoped tool credentials.

An isolated CONNECT proxy routes the unchanged GitHub URLs to a private TLS
fixture. No external API, model or production request is made by these controls.
"""

import argparse
from contextlib import contextmanager
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import io
import json
import os
from pathlib import Path
import select
import shutil
import socket
import socketserver
import ssl
import subprocess
import sys
import tempfile
import threading
import uuid
import zipfile


SOURCE = Path(__file__).resolve().parents[2]
TOKEN_NAMES = {"GH_TOKEN", "GITHUB_TOKEN"}


def load(path):
    spec = importlib.util.spec_from_file_location("asset_auth_profiles", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def clean_environment():
    return {key: value for key, value in os.environ.items()
            if key not in TOKEN_NAMES and key.lower() not in {"https_proxy", "http_proxy", "all_proxy", "no_proxy"}}


@contextmanager
def api_fixture(directory, expected_token, assets):
    openssl = shutil.which("openssl")
    if openssl is None and os.name == "nt":
        candidate = Path(os.environ.get("ProgramFiles", "C:/Program Files")) / "Git/usr/bin/openssl.exe"
        openssl = str(candidate) if candidate.is_file() else None
    if openssl is None:
        raise RuntimeError("The native TLS credential guard requires the existing OpenSSL tool")
    cert, key = directory / "tls.crt", directory / "tls.key"
    configuration = directory / "openssl.cnf"
    configuration.write_text('''[req]
distinguished_name=dn
x509_extensions=server
prompt=no
[dn]
CN=api.github.com
[server]
basicConstraints=critical,CA:TRUE
subjectKeyIdentifier=hash
authorityKeyIdentifier=keyid:always
keyUsage=critical,digitalSignature,keyEncipherment,keyCertSign,cRLSign
extendedKeyUsage=serverAuth
subjectAltName=DNS:api.github.com,DNS:assets.invalid
''', encoding="utf-8")
    subprocess.run([openssl, "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                    "-config", str(configuration),
                    "-keyout", str(key), "-out", str(cert)],
                   check=True, capture_output=True, timeout=30, env=clean_environment())
    state = {"requests": [], "authenticated": True}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def reply(self, code, body=b"", location=None):
            self.send_response(code)
            if location:
                self.send_header("Location", location)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            api = self.headers.get("Host") == "api.github.com"
            authorization = self.headers.get("Authorization")
            expected = "Bearer " + expected_token if state["authenticated"] else None
            state["requests"].append({"api": api, "path": self.path,
                                      "authorizationPresent": authorization is not None,
                                      "expectedAuthorization": authorization == (expected if api else None)})
            if authorization != (expected if api else None):
                self.reply(403, b"controlled credential scope refusal")
                return
            if api and "/releases/tags/" in self.path:
                repository, tag = self.path.removeprefix("/repos/").split("/releases/tags/")
                selected = [(index, item) for index, item in enumerate(assets, 1)
                            if item["repository"] == repository and item["tag"] == tag]
                self.reply(200, json.dumps({"tag_name": tag, "assets": [
                    {"id": index, "name": item["filename"]} for index, item in selected]}).encode())
            elif api and "/releases/assets/" in self.path:
                index = int(self.path.rsplit("/", 1)[1])
                self.reply(302, location=f"https://assets.invalid/{index}")
            elif not api:
                index = int(self.path.strip("/"))
                self.reply(200, assets[index - 1]["body"])
            else:
                self.reply(404)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(cert, key)
    server.socket = context.wrap_socket(server.socket, server_side=True)

    class Proxy(socketserver.StreamRequestHandler):
        def handle(self):
            first = self.rfile.readline().decode("ascii").strip()
            while self.rfile.readline().strip():
                pass
            if first not in {"CONNECT api.github.com:443 HTTP/1.0", "CONNECT api.github.com:443 HTTP/1.1",
                             "CONNECT assets.invalid:443 HTTP/1.0", "CONNECT assets.invalid:443 HTTP/1.1"}:
                return
            with socket.create_connection(server.server_address, timeout=30) as upstream:
                self.wfile.write(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                self.wfile.flush()
                while True:
                    readable, _, _ = select.select([self.connection, upstream], [], [], 30)
                    if not readable:
                        return
                    for incoming in readable:
                        data = incoming.recv(65536)
                        if not data:
                            return
                        (upstream if incoming is self.connection else self.connection).sendall(data)

    class ProxyServer(socketserver.ThreadingTCPServer):
        daemon_threads = True

    proxy = ProxyServer(("127.0.0.1", 0), Proxy)
    threads = [threading.Thread(target=item.serve_forever, daemon=True) for item in (server, proxy)]
    for thread in threads:
        thread.start()
    try:
        yield state, {"https_proxy": f"http://127.0.0.1:{proxy.server_address[1]}",
                      "SSL_CERT_FILE": str(cert)}
    finally:
        for item in (proxy, server):
            item.shutdown()
            item.server_close()
        for thread in threads:
            thread.join(timeout=5)


def workflow_scope():
    workflow = (SOURCE / ".github/workflows/local-acceptance.yml").read_text(encoding="utf-8")
    native = workflow.split("\n  native:\n", 1)[1]
    job_environment = native.split("    steps:\n", 1)[0]
    assert all(name + ":" not in job_environment for name in TOKEN_NAMES)
    steps = native.split("      - name: ")
    scoped = [step for step in steps if any(name + ":" in step for name in TOKEN_NAMES)]
    assert len(scoped) == 1 and "cross_platform.py --prepare-tools" in scoped[0]
    assert "GH_TOKEN: ${{ github.token }}" in scoped[0]
    assert "ACCEPTANCE_NATIVE_CHECKS: ${{ join(matrix.checks, ' ') }}" in scoped[0]
    candidate = next(step for step in steps if step.startswith("Run the same native commands"))
    assert "cross_platform.py --prepare-tools" not in candidate
    assert all(name + ":" not in candidate for name in TOKEN_NAMES)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--executable", type=Path, required=True)
    parser.add_argument("--source-receipt", type=Path, required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    profiles = load(SOURCE / ".github/local-acceptance/checks.py")
    source_receipt = json.loads(args.source_receipt.read_text())
    archive = args.executable.parent / source_receipt["url"].rsplit("/", 1)[1]
    assert hashlib.sha256(archive.read_bytes()).hexdigest() == source_receipt["archive_sha256"]
    assets = [{"repository": "fixture/tools", "tag": "v1", "filename": "tool.bin", "body": b"scoped-tool-payload"},
              {"repository": "nextest-rs/nextest", "tag": "cargo-nextest-" + source_receipt["version"],
               "filename": archive.name, "body": archive.read_bytes()}]
    protoc = profiles.cohost_protoc()
    if protoc is not None:
        folder = protoc.parent.parent
        # The existing native guard copies only the version-verified executable
        # into isolated homes. A cached original ZIP is not part of its contract.
        # Repackage those exact bytes for the controlled transport/extract test.
        protoc_archive = io.BytesIO()
        with zipfile.ZipFile(protoc_archive, 'w', compression=zipfile.ZIP_DEFLATED) as bundle:
            bundle.writestr('bin/' + protoc.name, protoc.read_bytes())
        assets.append({"repository": "protocolbuffers/protobuf", "tag": "v3.20.2",
                       "filename": "protoc-3.20.2-" + folder.name + ".zip",
                       "body": protoc_archive.getvalue()})
    workflow_scope()
    token = "controlled-asset-token-" + uuid.uuid4().hex
    results = []
    with tempfile.TemporaryDirectory(prefix="scode-asset-auth-") as temporary:
        private = Path(temporary)
        driver = private / "download.py"
        driver.write_text('''import importlib.util, os, sys
from pathlib import Path
spec = importlib.util.spec_from_file_location("profiles", sys.argv[1])
profiles = importlib.util.module_from_spec(spec)
spec.loader.exec_module(profiles)
token = os.environ.pop("GH_TOKEN", None)
os.environ.pop("GITHUB_TOKEN", None)
profiles.github_asset_download("fixture/tools", "v1", "tool.bin", Path(sys.argv[2]), token=token)
assert Path(sys.argv[2]).read_bytes() == b"scoped-tool-payload"
''')
        with api_fixture(private, token, assets) as (state, transport):
            environment = clean_environment() | transport

            def run(label, command, env, passed=True):
                state["requests"].clear()
                process = subprocess.run(list(map(str, command)), cwd=private, env=env,
                                         capture_output=True, text=True, encoding="utf-8", timeout=90)
                combined = process.stdout + process.stderr
                leaked = token in combined
                (output / (label + ".log")).write_text(combined.replace(token, "[REDACTED]"), encoding="utf-8")
                assert not leaked, "Tool credential appeared in subprocess output"
                assert (process.returncode == 0) == passed, f"Unexpected {label} outcome; inspect sanitized log"
                results.append({"case": label, "exitCode": process.returncode,
                                "expectedSuccess": passed, "requests": list(state["requests"]), "tokenLogged": False})
                return results[-1]

            checks = SOURCE / ".github/local-acceptance/checks.py"
            authenticated = environment | {"GH_TOKEN": token, "GITHUB_TOKEN": "unused-fallback"}
            result = run("authenticated-api-and-redirect", [sys.executable, driver, checks, private / "payload"], authenticated)
            assert len(result["requests"]) == 3 and all(row["expectedAuthorization"] for row in result["requests"])
            assert [row["authorizationPresent"] for row in result["requests"]] == [True, True, False]
            state["authenticated"] = False
            result = run("explicit-public-download", [sys.executable, driver, checks, private / "public-payload"], environment)
            assert len(result["requests"]) == 3 and not any(row["authorizationPresent"] for row in result["requests"])
            state["authenticated"] = True
            source = checks.read_text(encoding="utf-8")
            mutations = {
                "missing-metadata-auth": source.replace("if token:\n", "if token and accept == 'application/octet-stream':\n", 1),
                "missing-binary-auth": source.replace("if token:\n", "if token and accept == 'application/vnd.github+json':\n", 1),
                "redirect-credential-leak": source.replace("request.add_unredirected_header(", "request.add_header(", 1),
            }
            for label, mutated in mutations.items():
                assert mutated != source
                path = private / (label + ".py")
                path.write_text(mutated, encoding="utf-8")
                result = run(label, [sys.executable, driver, path, private / "refused-payload"], authenticated, passed=False)
                assert any(not row["expectedAuthorization"] for row in result["requests"])
                assert "HTTP Error 403" in (output / (label + ".log")).read_text()

            home = private / "cold-home"
            home.mkdir()
            preparation_output = private / "preparation"
            prepared_env = authenticated | {"HOME": str(home), "USERPROFILE": str(home),
                "ACCEPTANCE_NATIVE_CHECKS": "native-workspace native-cohost", "ACCEPTANCE_OUTPUT": str(preparation_output)}
            entry = SOURCE / ".github/local-acceptance/cross_platform.py"
            result = run("trusted-cold-tool-preparation", [sys.executable, entry, "--prepare-tools"], prepared_env)
            assert len(result["requests"]) == (6 if protoc is not None else 3)
            assert all(row["expectedAuthorization"] for row in result["requests"])
            assert json.loads((preparation_output / "tool-preparation/nextest-source.json").read_text()) == source_receipt
            result = run("candidate-step-refuses-token", [sys.executable, entry], prepared_env, passed=False)
            assert not result["requests"] and "separate --prepare-tools step" in (output / "candidate-step-refuses-token.log").read_text()

        probe = private / "candidate.py"
        probe.write_text('''import json, os, sys
from pathlib import Path
present = sorted(name for name in ("GH_TOKEN", "GITHUB_TOKEN") if name in os.environ)
Path(sys.argv[1]).write_text(json.dumps({"credentialNamesPresent": present}))
assert not present, "Candidate inherited GitHub authority"
''')
        executor = private / "execute.py"
        executor.write_text('''import importlib.util, os, sys
from pathlib import Path
spec = importlib.util.spec_from_file_location("executor", sys.argv[1])
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)
helper.run_command([sys.executable, sys.argv[2], sys.argv[3]], Path(sys.argv[4]), Path(sys.argv[5]),
                   {"GH_TOKEN": os.environ["GH_TOKEN"], "GITHUB_TOKEN": os.environ["GITHUB_TOKEN"]})
''')
        helper = SOURCE / ".github/scripts/local_acceptance.py"
        probe_result = output / "candidate-environment.json"
        result = run("candidate-subprocess-isolation", [sys.executable, executor, helper, probe, probe_result,
                     private, output / "candidate-command.log"], clean_environment() | {"GH_TOKEN": token, "GITHUB_TOKEN": token})
        assert json.loads(probe_result.read_text()) == {"credentialNamesPresent": []}
        original = helper.read_text(encoding="utf-8")
        cleanup = '    for name in ("GH_TOKEN", "GITHUB_TOKEN"):\n        environment.pop(name, None)\n'
        assert original.count(cleanup) == 1
        unsafe_helper = private / "unsafe-executor.py"
        unsafe_helper.write_text(original.replace(cleanup, ""), encoding="utf-8")
        run("candidate-inheritance-refused", [sys.executable, executor, unsafe_helper, probe, probe_result,
            private, output / "refused-candidate-command.log"],
            clean_environment() | {"GH_TOKEN": token, "GITHUB_TOKEN": token}, passed=False)
        assert json.loads(probe_result.read_text()) == {"credentialNamesPresent": ["GH_TOKEN", "GITHUB_TOKEN"]}
    (output / "receipt.json").write_text(json.dumps({"passed": True, "cases": results,
        "workflowTokenScopedToPreparation": True, "realHttpsViaPrivateConnectProxy": True,
        "nextestArchiveFixture": "original checksum-pinned archive",
        "protocArchiveFixture": "controlled ZIP of the existing version-verified executable" if protoc is not None else None,
        "publicGithubRequestMade": False, "productionRequestMade": False}, indent=2) + "\n", encoding="utf-8")
    print("PASS: authenticated metadata/binary APIs, public access, redirect stripping, three real HTTP refusals, "
          "cold pinned tool preparation and credential-free native subprocess; workflow token is preparation-only")


if __name__ == "__main__":
    main()
