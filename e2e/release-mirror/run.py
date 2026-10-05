#!/usr/bin/env python3
"""Exercise the publisher and real installer through an interrupted local mirror."""

import hashlib
import http.server
import io
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tarfile
import tempfile
import threading

ROOT = Path(__file__).resolve().parents[2]


def main():
    with tempfile.TemporaryDirectory(prefix="scode-release-e2e-") as temporary:
        root = Path(temporary)
        public = root / "public"
        public.mkdir()
        requests = []

        class Handler(http.server.SimpleHTTPRequestHandler):
            def __init__(self, *args, **kwargs):
                super().__init__(*args, directory=str(public), **kwargs)

            def log_message(self, *_args):
                pass

            def do_GET(self):
                requests.append(self.path.split("?")[0])
                super().do_GET()

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            exercise(root, public, server.server_port, requests)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()
    print("release mirror interruption, retry, pinning and integrity checks passed")


def exercise(root, public, port, requests):
    shim = root / "bin"
    shim.mkdir()
    # Only the storage transport is faked. Both production entry points run
    # unchanged, including HTTP readback, SHA verification and tar extraction.
    (shim / "coscmd").write_text(
        """#!/usr/bin/env python3
import os, pathlib, shutil, sys
root = pathlib.Path(os.environ['MIRROR_TEST_ROOT'])
log = root / 'uploads.jsonl'
count = len(log.read_text().splitlines()) if log.exists() else 0
if count + 1 == int(os.environ.get('MIRROR_FAIL_AT', '0')):
    sys.exit(17)
assert sys.argv[1] == 'upload'
target = root / 'public' / sys.argv[3]
target.parent.mkdir(parents=True, exist_ok=True)
pending = target.with_suffix(target.suffix + '.pending')
shutil.copyfile(sys.argv[2], pending)
pending.replace(target)
with log.open('a') as stream:
    stream.write(sys.argv[3] + '\\n')
"""
    )
    (shim / "curl").write_text(
        """#!/usr/bin/env python3
import os, sys, urllib.request, urllib.error
args = sys.argv[1:]
url = args[-1]
base = os.environ['MIRROR_TEST_URL']
for remote, local in [
    ('https://api.github.com/repos/sudoprivacy/sudocode/releases', '/api'),
    ('https://github.com/sudoprivacy/sudocode/releases/download', '/github'),
    ('https://mirror.test', '')]:
    if url.startswith(remote + '/'):
        url = base + local + url[len(remote):]
        break
else:
    raise RuntimeError('unexpected external request: ' + url)
try:
    data = urllib.request.urlopen(url, timeout=10).read()
except urllib.error.HTTPError:
    sys.exit(22)
if '-o' in args:
    with open(args[args.index('-o') + 1], 'wb') as stream:
        stream.write(data)
else:
    sys.stdout.buffer.write(data)
"""
    )
    for path in shim.iterdir():
        path.chmod(0o755)
    env = {
        **os.environ,
        "PATH": str(shim) + os.pathsep + os.environ["PATH"],
        "MIRROR_TEST_ROOT": str(root),
        "MIRROR_TEST_URL": f"http://127.0.0.1:{port}",
        "SCODE_MIRROR": "https://mirror.test/sudocode/release/latest",
        "NO_COLOR": "1",
    }
    for name in ("SCODE_VERSION", "SCODE_INSTALL_DIR"):
        env.pop(name, None)

    def run(args, *, success=True, extra=None):
        result = subprocess.run(
            args, env={**env, **(extra or {})}, capture_output=True, text=True, timeout=60
        )
        assert (result.returncode == 0) == success, result.stdout + result.stderr
        return result

    def prepare(version, *, is_bundle=False):
        dist = root / version
        dist.mkdir()
        for target in ("linux-x64", "linux-arm64", "macos-x64", "macos-arm64"):
            content = f"#!/bin/sh\necho {version}\n".encode()
            with tarfile.open(dist / f"scode-{target}.tar.gz", "w:gz") as archive:
                info = tarfile.TarInfo(f"scode-{target}/scode")
                info.size, info.mode = len(content), 0o755
                archive.addfile(info, io.BytesIO(content))
        for arch in ("x64", "arm64"):
            (dist / f"scode-windows-{arch}.zip").write_bytes(version.encode())
        (dist / f"scode_{version[1:]}_amd64.deb").write_bytes(version.encode())
        if is_bundle:
            shutil.copyfile(
                dist / "scode-linux-x64.tar.gz", dist / "scode-linux-x64-bundle.tar.gz"
            )
        checksums = "".join(
            f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.name}\n"
            for path in sorted(dist.iterdir())
        )
        (dist / "SHA256SUMS.txt").write_text(checksums)
        github = public / "github" / version
        shutil.copytree(dist, github)
        tags = public / "api/tags"
        tags.mkdir(parents=True, exist_ok=True)
        (tags / version).write_text(json.dumps({"tag_name": version}))
        (public / "api/latest").write_text(json.dumps({"tag_name": version}))

    def publish(phase, version, **kwargs):
        return run(
            [
                "python3", str(ROOT / "scripts/publish_release_mirror.py"), phase,
                "--version", version, "--dist", str(root / version),
                "--installer", str(ROOT / "install.sh"),
                "--public-base", env["MIRROR_TEST_URL"],
            ],
            **kwargs,
        )

    installed = root / "installed"

    def install(expected, *flags, success=True):
        result = run(
            ["sh", str(ROOT / "install.sh"), "--prefix", str(installed), "--no-sudo", *flags],
            success=success,
        )
        if success:
            assert run([str(installed / "scode")]).stdout.strip() == expected
        return result

    def uploads():
        return (root / "uploads.jsonl").read_text().splitlines()

    pointer = public / "sudocode/release/latest/version.txt"
    prepare("v1.0.0")
    # Before migration there is no pointer: use the same GitHub version for both files.
    install("v1.0.0")
    install("v1.0.0", "--version", "v1.0.0")
    publish("stage", "v1.0.0")
    assert not pointer.exists()
    publish("promote", "v1.0.0")
    install("v1.0.0")
    prepare("v1.0.1", is_bundle=True)
    dist = root / "v1.0.1"
    manifest = dist / "SHA256SUMS.txt"
    original_manifest = manifest.read_text()
    before = len(uploads())
    extra = dist / "scode-unrecognized.tar.gz"
    extra.write_bytes(b"unexpected archive")
    manifest.write_text(
        original_manifest + f"{hashlib.sha256(extra.read_bytes()).hexdigest()}  {extra.name}\n"
    )
    assert "release artifact set differs" in publish("stage", "v1.0.1", success=False).stderr
    extra.unlink()
    manifest.write_text("".join(
        line for line in original_manifest.splitlines(True)
        if "scode-linux-arm64.tar.gz" not in line
    ))
    assert "release artifact set differs" in publish("stage", "v1.0.1", success=False).stderr
    manifest.write_text(original_manifest)
    assert len(uploads()) == before, "invalid manifests must fail before uploading"
    bundle = dist / "scode-linux-x64-bundle.tar.gz"
    original_bundle = bundle.read_bytes()
    bundle.write_bytes(b"corrupt bundle")
    assert "local checksum mismatch" in publish("stage", "v1.0.1", success=False).stderr
    assert len(uploads()) == before
    bundle.write_bytes(original_bundle)
    before = len(uploads())
    publish("stage", "v1.0.1", success=False, extra={"MIRROR_FAIL_AT": str(before + 4)})
    staged = uploads()[before:]
    assert len(staged) == 3
    requests.clear()
    install("v1.0.0")
    assert "/api/latest" not in requests
    assert "/github/v1.0.0/SHA256SUMS.txt" in requests
    publish("promote", "v1.0.1", success=False)
    assert pointer.read_text().strip() == "v1.0.0"
    publish("stage", "v1.0.1")
    assert all(uploads().count(key) == 1 for key in staged), "retry replaced matching assets"
    # Even interruption after the new installer upload leaves a complete old release.
    publish("promote", "v1.0.1", success=False, extra={"MIRROR_FAIL_AT": str(len(uploads()) + 2)})
    install("v1.0.0")
    publish("promote", "v1.0.1")
    assert (public / "sudocode/release/v1.0.1" / bundle.name).read_bytes() == original_bundle
    install("v1.0.1")
    install("v1.0.0", "--version", "v1.0.0")
    publish("promote", "v1.0.0")
    assert pointer.read_text().strip() == "v1.0.1", "old retry rolled latest backward"
    install("v1.0.1")
    target_os = "macos" if platform.system() == "Darwin" else "linux"
    arch = "arm64" if platform.machine() in ("arm64", "aarch64") else "x64"
    archive = public / f"sudocode/release/v1.0.1/scode-{target_os}-{arch}.tar.gz"
    archive.write_bytes(b"corrupt mirror")
    result = install(None, success=False)
    assert "checksum mismatch" in result.stderr
    assert run([str(installed / "scode")]).stdout.strip() == "v1.0.1"
    result = publish("stage", "v1.0.1", success=False)
    assert "refusing to replace immutable object" in result.stderr
    assert pointer.read_text().strip() == "v1.0.1"


if __name__ == "__main__":
    main()
