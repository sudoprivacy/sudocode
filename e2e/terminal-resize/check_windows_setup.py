"""Exercise the installer subprocess with a real pinned distribution and bad HTTP bodies."""

import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import mmap
import os
from pathlib import Path
import runpy
import shutil
import subprocess
import sys
import tempfile
import threading
import time


# Redirect only the download boundary; run the unchanged installer entry point
# in a child process with actual HTTP, ZIP extraction and filesystem writes.
INSTALL = """import runpy, sys, urllib.request
real_open = urllib.request.urlopen
official, local, installer, destination = sys.argv[1:]
def controlled_open(url, *args, **kwargs):
    assert url == official, url
    return real_open(local, *args, **kwargs)
urllib.request.urlopen = controlled_open
sys.argv = [installer, destination]
runpy.run_path(installer, run_name='__main__')
"""


def run_concurrent_installers(command):
    processes = []
    try:
        for _ in range(2):
            processes.append(subprocess.Popen(
                command, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            ))
        for process in processes:
            stdout, stderr = process.communicate(timeout=120)
            assert process.returncode == 0, stdout + stderr
    finally:
        for process in processes:
            if process.poll() is None:
                process.terminate()
            process.communicate(timeout=10)


def main():
    installer = Path(__file__).with_name("setup_windows.py")
    settings = runpy.run_path(str(installer))
    commit = settings["COMMIT"]
    archive = Path(sys.argv[1]).resolve()
    with archive.open("rb") as source:
        assert hashlib.file_digest(source, "sha256").hexdigest() == settings["SHA256"]
    official = f"https://update.code.visualstudio.com/commit:{commit}/win32-x64-archive/stable"
    counts = {"recover": 0, "reject": 0, "concurrent": 0}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_GET(self):
            label = self.path.strip("/")
            counts[label] += 1
            good = label == "concurrent" or (label == "recover" and counts[label] > 1)
            self.send_response(200)
            self.send_header("Content-Length", str(archive.stat().st_size if good else 7))
            self.end_headers()
            if good:
                with archive.open("rb") as source:
                    shutil.copyfileobj(source, self.wfile)
            else:
                self.wfile.write(b"corrupt")

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="scode-terminal-installer-") as temporary:
            root = Path(temporary)
            for label in ("recover", "reject"):
                destination = root / label
                proc = subprocess.run(
                    [
                        sys.executable, "-c", INSTALL, official,
                        f"http://127.0.0.1:{server.server_port}/{label}",
                        str(installer), str(destination),
                    ],
                    capture_output=True, text=True, timeout=120,
                )
                assert "checksum mismatch" in proc.stdout, proc.stdout + proc.stderr
                assert not (destination / "host.partial").exists()
                if label == "recover":
                    assert proc.returncode == 0 and counts[label] == 2, proc.stdout + proc.stderr
                    with (destination / "Code.exe").open("rb") as executable:
                        assert executable.read(2) == b"MZ"
                    package = destination / commit[:10] / "resources/app"
                    product = json.loads((package / "product.json").read_text(encoding="utf-8"))
                    assert product["commit"] == commit
                    assert (package / "node_modules.asar").stat().st_size > 0
                    assert list((package / "node_modules.asar.unpacked/node-pty").rglob("*.node"))
                    executable = destination / "Code.exe"
                    icu = destination / commit[:10] / "icudtl.dat"
                    stable = [executable, icu, package / "node_modules.asar"]
                    modified = {path: path.stat().st_mtime_ns for path in stable}
                    host = None
                    try:
                        if os.name == "nt":
                            ready = destination / "host-ready"
                            host = subprocess.Popen(
                                [str(executable), "-e", "Intl.DateTimeFormat('en').format(new Date());"
                                 "require('fs').writeFileSync(process.argv[1], 'ready');"
                                 "setInterval(() => {}, 1000)", str(ready)],
                                env={**os.environ, "ELECTRON_RUN_AS_NODE": "1"},
                                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
                            )
                            deadline = time.monotonic() + 10
                            while not ready.exists() and host.poll() is None and time.monotonic() < deadline:
                                time.sleep(0.05)
                            assert ready.exists() and host.poll() is None, "Native terminal host did not start"
                        # A mapped ICU file reproduces Windows' real Errno 22.
                        # Both installers must preserve it while repairing metadata.
                        (package / "product.json").write_text("corrupt", encoding="utf-8")
                        with icu.open("rb") as data, mmap.mmap(data.fileno(), 0, access=mmap.ACCESS_READ):
                            run_concurrent_installers([sys.executable, str(installer), str(destination)])
                        assert counts[label] == 2
                        assert modified == {path: path.stat().st_mtime_ns for path in stable}
                        assert json.loads((package / "product.json").read_text(encoding="utf-8"))["commit"] == commit
                        if host:
                            assert host.poll() is None, "Cache setup stopped the running terminal host"
                    finally:
                        if host:
                            if host.poll() is None:
                                host.terminate()
                            host.wait(timeout=10)
                            host.stderr.close()
                    print("Installer in-use concurrent cache acceptance passed", flush=True)
                    # Corrupted cache entries also fail closed, without being
                    # silently accepted or initiating an unobserved download.
                    (destination / "host.zip").write_bytes(b"corrupt")
                    corrupt = subprocess.run(
                        [sys.executable, str(installer), str(destination)],
                        capture_output=True, text=True, timeout=120,
                    )
                    assert corrupt.returncode != 0 and "checksum mismatch" in corrupt.stderr
                    assert counts[label] == 2
                else:
                    assert proc.returncode != 0 and counts[label] == 3, proc.stdout + proc.stderr
                    assert not (destination / "host.zip").exists()
                    assert not (destination / "Code.exe").exists()
                print(f"Installer HTTP acceptance passed: {label}, {counts[label]} requests", flush=True)
            destination = root / "concurrent"
            run_concurrent_installers(
                [sys.executable, "-c", INSTALL, official,
                 f"http://127.0.0.1:{server.server_port}/concurrent", str(installer), str(destination)],
            )
            assert counts["concurrent"] == 1, counts
            assert json.loads((destination / commit[:10] / "resources/app/product.json").read_text(encoding="utf-8"))["commit"] == commit
            print("Installer concurrent cold-cache acceptance passed: one download", flush=True)
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
