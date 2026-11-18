"""Real TLS/Git/archive transport controls over the existing compiled VFS fixture.

Invoked by vfs_search_cache_acceptance after its original native Cargo journey;
this never replaces the real co-host job or supplies product coverage credit.
"""

import copy
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import io
import json
import os
from pathlib import Path
import select
import shutil
import socket
import ssl
import subprocess
import sys
import tarfile
import threading
import zipfile


def run(source_root, root, output, journey, command, environment):
    output.mkdir(parents=True, exist_ok=False)
    sys.path.insert(0, str(source_root / ".github/scripts"))
    import vfs_search_publication as publication
    for relative in publication.SOURCES + [".github/local-acceptance/checks.py"]:
        if relative == "rust/Cargo.lock":
            continue  # This is the real private Git/Cargo fixture's source pin.
        destination = root / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(source_root / relative, destination)
    command("git", "init", "--initial-branch=main", str(root))
    command("git", "config", "user.name", "Compiler publication fixture")
    command("git", "config", "user.email", "compiler@example.invalid")
    command("git", "add", "--", *publication.SOURCES, ".github/local-acceptance/checks.py")
    command("git", "commit", "-m", "Immutable compiler publication source fixture")
    head = command("git", "rev-parse", "HEAD").read_text().strip()
    env = {key: value for key, value in os.environ.items() if key.upper() not in {"GH_TOKEN", "GITHUB_TOKEN"}}
    env.update(environment, GITHUB_REPOSITORY=publication.REPOSITORY, GITHUB_SHA=head, GITHUB_RUN_ID="101",
               GITHUB_RUN_ATTEMPT="1", GITHUB_JOB="cohost-live", GITHUB_EVENT_NAME="merge_group",
               PYTHONUTF8="1", PYTHONIOENCODING="utf-8")
    records = []
    entry = source_root / ".github/scripts/vfs_search_publication.py"

    def execute(label, args, *, success=True, overrides=None, expected=None):
        log = output / f"{label}.log"
        result = subprocess.run([sys.executable, str(entry), *map(str, args), "--root", str(root)],
                                cwd=root, env={**env, **(overrides or {})}, capture_output=True, timeout=120)
        log.write_bytes(result.stdout + result.stderr)
        assert (result.returncode == 0) == success, log.read_text(errors="replace")
        assert b"PUBLICATION_DUMMY_TOKEN" not in log.read_bytes()
        matched = None
        if expected:
            messages = (expected,) if isinstance(expected, str) else expected
            matched = next((message for message in messages if message in log.read_text(errors="replace")), None)
            assert matched is not None, log.read_text(errors="replace")
        row = {"case": label, "exit_code": result.returncode, "log_sha256": publication.sha(log)}
        if matched is not None:
            row["refusal"] = matched
        records.append(row)
        print("VFS_PUBLICATION_CONTROL=" + json.dumps(row, sort_keys=True), flush=True)

    exported = output / "exported"
    execute("export", ["export", "--output", exported, "--journey", journey])
    manifest_path = exported / "identity/manifest.json"
    original = json.loads(manifest_path.read_bytes())
    execute("warm-metadata-only", ["export", "--output", output / "lookup", "--journey", journey,
                                  "--include-archive", "false"])
    assert json.loads((output / "lookup/identity/manifest.json").read_bytes())["archive"] is None
    assert not (output / "lookup/data").exists()
    source = root / ".native-vfs"
    # An exact sub-second source mtime is retained by the real PAX transport.
    stamp = source / ".ci-cache/fixture-timestamp"
    stamp.write_bytes(b"exact native compiler timestamp\n")
    os.utime(stamp, ns=(1_700_000_000_123_456_700, 1_700_000_000_123_456_700))
    execute("export-nanoseconds", ["export", "--output", output / "precise", "--journey", journey])
    exported = output / "precise"
    manifest_path = exported / "identity/manifest.json"
    original = json.loads(manifest_path.read_bytes())
    token = "PUBLICATION_DUMMY_TOKEN"
    requests, state = [], {"mode": "valid"}
    repo = {"id": 17, "full_name": publication.REPOSITORY}
    original_run = {"id": 101, "run_attempt": 1, "head_sha": head, "event": "merge_group",
                    "path": publication.WORKFLOW, "repository": repo, "head_repository": repo,
                    "status": "completed", "conclusion": "success"}
    names = ["Search authorization and restart under the confined co-host owner",
             "The driver-ai co-host still resolves", publication.EXPORT_STEP,
             "Upload queue compiler identity", "Upload queue compiler data"]

    def metadata():
        document = copy.deepcopy(original)
        if state["mode"] == "wrong-source":
            document["sources"][publication.WORKFLOW] = "f" * 64
        if state["mode"] == "wrong-key":
            document["key"] = document["key"].replace("vfs-search-v2-", "vfs-search-v1-")
        if state["mode"] == "wrong-command":
            document["journey"]["command"]["command"].append("--ignored")
        payload = io.BytesIO()
        with zipfile.ZipFile(payload, "w") as archive:
            archive.writestr(zipfile.ZipInfo("manifest.json", date_time=(2000, 1, 1, 0, 0, 0)), json.dumps(document))
        return payload.getvalue()

    def artifacts():
        rows = []
        for kind, number in [("identity", 201), ("compiler", 202)]:
            rows.append({"id": number, "name": publication.artifact_name(kind, original_run), "expired": False,
                         "digest": "sha256:" + hashlib.sha256(metadata()).hexdigest(),
                         "workflow_run": {"id": 101, "head_sha": head, "repository_id": 17, "head_repository_id": 17}})
        if state["mode"] == "duplicate-artifact":
            rows.append(copy.deepcopy(rows[0]))
        if state["mode"] == "bad-digest":
            rows[0]["digest"] = "sha256:" + "0" * 64
        if state["mode"] == "foreign-artifact":
            rows[0]["workflow_run"]["head_repository_id"] = 99
        if state["mode"] == "expired":
            rows[0]["expired"] = True
        if state["mode"] == "data-without-identity":
            rows = rows[1:]
        if state["mode"] == "absent-export":
            rows = []
        return rows

    certificate, private_key = output / "tls.pem", output / "tls.key"
    config = output / "tls.conf"
    config.write_text("[req]\ndistinguished_name=dn\nx509_extensions=x\nprompt=no\n[dn]\nCN=api.github.com\n"
                      "[x]\nsubjectAltName=DNS:api.github.com,DNS:storage.fixture.invalid\n"
                      "basicConstraints=critical,CA:TRUE\nkeyUsage=critical,keyCertSign,digitalSignature,keyEncipherment\n"
                      "extendedKeyUsage=serverAuth\n")
    openssl = shutil.which("openssl")
    if not openssl and os.name == "nt":
        openssl = str(Path(shutil.which("git")).resolve().parents[1] / "usr/bin/openssl.exe")
    subprocess.run([openssl, "req", "-new", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
                    "-keyout", str(private_key), "-out", str(certificate), "-config", str(config)],
                   check=True, capture_output=True)

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            host = self.headers.get("Host")
            authorized = self.headers.get("Authorization") == f"Bearer {token}"
            requests.append({"host": host, "path": self.path, "authorization_present": bool(self.headers.get("Authorization"))})
            if host == "storage.fixture.invalid":
                payload = metadata()
            elif host != "api.github.com" or not authorized:
                self.send_error(401)
                return
            elif self.path.endswith("/zip"):
                self.send_response(302)
                self.send_header("Location", "https://storage.fixture.invalid/manifest.zip")
                self.end_headers()
                return
            else:
                run = copy.deepcopy(original_run)
                if state["mode"] == "failed-latest":
                    run["conclusion"] = "failure"
                if state["mode"] == "changed-attempt":
                    run["run_attempt"] = 2
                if state["mode"] == "foreign-run":
                    run["repository"] = {"id": 99, "full_name": "fixture/foreign"}
                if "/workflows/" in self.path:
                    value = {"total_count": 1, "workflow_runs": [run]}
                elif "/jobs?" in self.path:
                    steps = [{"name": name, "conclusion": "success"} for name in names]
                    if state["mode"] == "failed-step":
                        steps[0]["conclusion"] = "failure"
                    job = {"name": publication.JOB, "conclusion": "success", "status": "completed",
                           "run_id": 101, "run_attempt": 1, "head_sha": head, "steps": steps}
                    if state["mode"] == "wrong-job-attempt":
                        job["run_attempt"] = 2
                    if state["mode"] == "failed-job":
                        job["conclusion"] = "failure"
                    value = {"total_count": 1, "jobs": [job]}
                elif "/artifacts?" in self.path:
                    rows = artifacts()
                    value = {"total_count": len(rows), "artifacts": rows}
                else:
                    value = run
                payload = json.dumps(value).encode()
            self.send_response(200)
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)

        def log_message(self, *_args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(certificate, private_key)
    server.socket = context.wrap_socket(server.socket, server_side=True)

    class Proxy(BaseHTTPRequestHandler):
        def do_CONNECT(self):
            if self.path not in {"api.github.com:443", "storage.fixture.invalid:443"}:
                self.send_error(403)
                return
            with socket.create_connection(("127.0.0.1", server.server_port)) as upstream:
                self.send_response(200)
                self.end_headers()
                while True:
                    readable, _, _ = select.select([self.connection, upstream], [], [], 5)
                    if not readable:
                        return
                    for current in readable:
                        data = current.recv(65536)
                        if not data:
                            return
                        (upstream if current is self.connection else self.connection).sendall(data)

        def log_message(self, *_args):
            pass

    proxy = ThreadingHTTPServer(("127.0.0.1", 0), Proxy)
    for item in (server, proxy):
        threading.Thread(target=item.serve_forever, daemon=True).start()
    main_env = {"GH_TOKEN": token, "Gh_ToKeN": token, "GITHUB_EVENT_NAME": "push", "GITHUB_REF": "refs/heads/main",
                "SSL_CERT_FILE": str(certificate), "HTTPS_PROXY": f"http://127.0.0.1:{proxy.server_port}",
                "HTTP_PROXY": f"http://127.0.0.1:{proxy.server_port}", "NO_PROXY": "",
                "ImageVersion": "20261111.82.6", "GITHUB_OUTPUT": str(output / "github-output")}
    try:
        selected = output / "selected"
        execute("valid-current-producer", ["select", "--output", selected], overrides=main_env)
        selection = json.loads((selected / "selection.json").read_bytes())
        assert selection["key"] == original["key"]
        assert original["inputs"]["environment"]["ImageVersion"] != main_env["ImageVersion"]
        for mode in ["failed-latest", "foreign-run", "failed-step", "duplicate-artifact", "foreign-artifact",
                     "bad-digest", "expired", "wrong-source", "wrong-key", "wrong-command",
                     "wrong-job-attempt", "failed-job", "data-without-identity"]:
            state["mode"] = mode
            execute(mode, ["select", "--output", output / mode], overrides=main_env, success=False)
        state["mode"] = "absent-export"
        execute("old-producer-fallback", ["select", "--output", output / "absent-export"], overrides=main_env)
        assert json.loads((output / "absent-export/selection.json").read_bytes())["available"] is False
        state["mode"] = "valid"
        saved = output / "original-native-inputs"
        source.rename(saved)
        execute("import-original-image-key", ["import", "--output", selected, "--data", exported / "data"], overrides=main_env)
        assert stamp.stat().st_mtime_ns == 1_700_000_000_123_456_700
        refused = command(sys.executable, str(source_root / ".github/scripts/vfs_search_cache.py"), "build", "--root", str(root),
                          "--output", str(output / "wrong-image-build"),
                          extra={"ImageVersion": main_env["ImageVersion"]}, success=False)
        assert "Restored VFS compiler context differs" in refused.read_text()
        command(sys.executable, str(source_root / ".github/scripts/vfs_search_cache.py"), "build", "--root", str(root),
                "--output", str(output / "matching-image-build"))
        warm = json.loads((output / "matching-image-build/commands.json").read_bytes())
        assert all(row["compiler_artifacts"] and all(item["fresh"] for item in row["compiler_artifacts"]) for row in warm), warm
        command(sys.executable, str(source_root / ".github/scripts/vfs_search_cache.py"), "test", "--root", str(root),
                "--output", str(output / "matching-image-test"), extra=environment)
        source.rename(output / "qualified-imported-native-inputs")
        state["mode"] = "changed-attempt"
        execute("changed-attempt-refused", ["import", "--output", selected, "--data", exported / "data"],
                overrides=main_env, success=False, expected="no longer current")
        state["mode"] = "valid"
        for label, name, kind, mode in [("traversal", "../escape", tarfile.REGTYPE, 0o644),
                                        ("absolute", "/escape", tarfile.REGTYPE, 0o644),
                                        ("symlink", "link", tarfile.SYMTYPE, 0o777),
                                        ("hardlink", "link", tarfile.LNKTYPE, 0o644),
                                        ("special", "pipe", tarfile.FIFOTYPE, 0o644),
                                        ("setuid", "file", tarfile.REGTYPE, 0o4755),
                                        ("negative-size", "file", tarfile.REGTYPE, 0o644)]:
            bad = output / label
            bad.mkdir()
            archive = bad / "compiler.tar.gz"
            with tarfile.open(archive, "w:gz") as stream:
                info = tarfile.TarInfo(name)
                info.type, info.mode = kind, mode
                if label == "negative-size":
                    # The normal writer rejects a negative regular-file size
                    # before emitting it. PAX can carry that malformed size to
                    # the real reader without asking the writer to copy data.
                    info.pax_headers["size"] = "-1"
                info.linkname = "../../escape" if kind in {tarfile.SYMTYPE, tarfile.LNKTYPE} else ""
                stream.addfile(info)
            document = copy.deepcopy(original)
            document["archive"].update(sha256=publication.sha(archive), size=archive.stat().st_size, members=1, bytes=1)
            publication.write(selected / "manifest.json", document)
            # Some supported tarfile versions discard an invalid negative-size
            # member before yielding it. The exact inventory guard must still
            # refuse that shortened archive; other versions reach the size guard.
            refusal = (("Unsafe compiler archive", "Compiler archive inventory differs from its producer")
                       if label == "negative-size" else "Unsafe compiler archive")
            execute(label, ["import", "--output", selected, "--data", bad], overrides=main_env,
                    success=False, expected=refusal)
            assert not source.exists() and not (root / "escape").exists()
        publication.write(selected / "manifest.json", original)
        altered = output / "altered"
        altered.mkdir()
        path = altered / "compiler.tar.gz"
        shutil.copyfile(exported / "data/compiler.tar.gz", path)
        with path.open("r+b") as stream:
            stream.write(b"changed")
        execute("changed-archive", ["import", "--output", selected, "--data", altered], overrides=main_env,
                success=False, expected="differs from its original producer")
        saved.rename(source)
        redirected = [item for item in requests if item["host"] == "storage.fixture.invalid"]
        assert redirected and not any(item["authorization_present"] for item in redirected)
        assert all(item["authorization_present"] for item in requests if item["host"] == "api.github.com")
    finally:
        for item in (proxy, server):
            item.shutdown()
            item.server_close()
        if private_key.resolve().parent != output.resolve():
            raise RuntimeError("Unexpected private fixture-key location")
        private_key.unlink(missing_ok=True)
        publication.write(output / "requests.json", requests)
    publication.write(output / "receipt.json", {"passed": True, "cases": records,
        "exact_producer_image_key_retained": True, "different_image_compilation_refused": True,
        "matching_image_normal_cargo_fresh_and_real_current_runtime_test": True,
        "nanosecond_timestamp_preserved": True, "cross_origin_credential_forwarded": False,
        "scope": "Real private Git/Cargo/native/tar/TLS compiler-input transport; no product coverage, signature or hosted cache qualification"})
