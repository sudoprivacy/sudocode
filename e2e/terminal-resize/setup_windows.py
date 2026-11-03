"""Install the checksum-pinned Electron/ConPTY/xterm runtime used by the PTY test."""

from contextlib import contextmanager
import errno
import hashlib
from http.client import IncompleteRead
import os
from pathlib import Path
import shutil
import sys
import tempfile
import time
import urllib.request
import zipfile

COMMIT = "07f806f999227108933c2e30515b26eecc1fda74"
SHA256 = "52f47072473375767d63ea5be9ffb96a3092124223fe5ce036834a299715014e"


@contextmanager
def cache_lock(root):
    # An OS-owned lock is released after a crash and also serializes downloads.
    with (root / ".install.lock").open("a+b") as lock:
        if os.name == "nt":
            import msvcrt
        else:
            import fcntl
        deadline = time.monotonic() + 120
        while True:
            try:
                lock.seek(0)
                if os.name == "nt":
                    msvcrt.locking(lock.fileno(), msvcrt.LK_NBLCK, 1)
                else:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except OSError as error:
                if error.errno not in (errno.EACCES, errno.EAGAIN, errno.EDEADLK):
                    raise
                if time.monotonic() >= deadline:
                    raise TimeoutError("Timed out waiting for the terminal cache installer") from error
                time.sleep(0.1)
        try:
            yield
        finally:
            lock.seek(0)
            if os.name == "nt":
                msvcrt.locking(lock.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(lock, fcntl.LOCK_UN)


def extract_member(package, member, root):
    target = root / member.filename
    if not target.resolve().is_relative_to(root):
        raise ValueError(f"Terminal cache member escapes its root: {member.filename}")
    if member.is_dir():
        target.mkdir(parents=True, exist_ok=True)
        return
    if target.is_file() and target.stat().st_size == member.file_size:
        with package.open(member) as expected, target.open("rb") as actual:
            while chunk := expected.read(1024 * 1024):
                if actual.read(len(chunk)) != chunk:
                    break
            else:
                # A running Electron host memory-maps ICU and executable files.
                # Keep identical bytes in place, including legacy cache entries.
                return
    target.parent.mkdir(parents=True, exist_ok=True)
    descriptor, name = tempfile.mkstemp(prefix=target.name + ".", suffix=".partial", dir=target.parent)
    temporary = Path(name)
    try:
        with os.fdopen(descriptor, "wb") as output, package.open(member) as source:
            shutil.copyfileobj(source, output)
        temporary.replace(target)
    finally:
        temporary.unlink(missing_ok=True)


def verify_archive(archive):
    with archive.open("rb") as data:
        actual = hashlib.file_digest(data, "sha256").hexdigest()
    if actual != SHA256:
        raise ValueError(
            f"terminal archive checksum mismatch: {actual} ({archive.stat().st_size} bytes)"
        )


def download_archive(archive):
    url = f"https://update.code.visualstudio.com/commit:{COMMIT}/win32-x64-archive/stable"
    temporary = archive.with_suffix(".partial")
    for attempt in range(1, 4):
        try:
            with urllib.request.urlopen(url, timeout=120) as response, temporary.open("wb") as out:
                shutil.copyfileobj(response, out)
            # A CDN may return a truncated or incorrect body without an HTTP
            # error. Never promote it to the cache before checking the pin.
            verify_archive(temporary)
            temporary.replace(archive)
            return
        except (OSError, ValueError, IncompleteRead) as error:
            temporary.unlink(missing_ok=True)
            if attempt == 3:
                raise
            print(f"Terminal download attempt {attempt}/3 failed: {error}; retrying", flush=True)
            time.sleep(attempt)


def main():
    root = Path(sys.argv[1]).resolve()
    root.mkdir(parents=True, exist_ok=True)
    with cache_lock(root):
        archive = root / "host.zip"
        if not archive.exists():
            download_archive(archive)
        else:
            verify_archive(archive)
        version = COMMIT[:10]
        with zipfile.ZipFile(archive) as package:
            # Node mode needs no editor extensions, language servers or updater.
            # Keep the native ABI from the same checksum-pinned distribution.
            for member in package.infolist():
                name = member.filename
                if name == "Code.exe" or (
                    name.startswith(f"{version}/") and name.count("/") == 1 and not name.endswith("/")
                ) or name in {
                    f"{version}/resources/app/node_modules.asar",
                    f"{version}/resources/app/product.json",
                    f"{version}/resources/app/package.json",
                } or name.startswith(f"{version}/resources/app/node_modules.asar.unpacked/node-pty/"):
                    extract_member(package, member, root)
    print(f"Verified terminal host: {root / 'Code.exe'}")


if __name__ == "__main__":
    main()
