"""Exercise the installer subprocess with a real pinned distribution and bad HTTP bodies."""

import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import runpy
import shutil
import subprocess
import sys
import tempfile
import threading


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


def main():
    installer = Path(__file__).with_name("setup_windows.py")
    settings = runpy.run_path(str(installer))
    commit = settings["COMMIT"]
    archive = Path(sys.argv[1]).resolve()
    with archive.open("rb") as source:
        assert hashlib.file_digest(source, "sha256").hexdigest() == settings["SHA256"]
    official = f"https://update.code.visualstudio.com/commit:{commit}/win32-x64-archive/stable"
    counts = {"recover": 0, "reject": 0}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def do_GET(self):
            label = self.path.strip("/")
            counts[label] += 1
            good = label == "recover" and counts[label] > 1
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
                    cached = subprocess.run(
                        [sys.executable, str(installer), str(destination)],
                        capture_output=True, text=True, timeout=120,
                    )
                    assert cached.returncode == 0 and counts[label] == 2, cached.stdout + cached.stderr
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
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
