"""Install the checksum-pinned Electron/ConPTY/xterm runtime used by the PTY test."""

from contextlib import contextmanager
import argparse
import errno
import hashlib
from http.client import IncompleteRead
import os
from pathlib import Path
import platform
import shutil
import sys
import tempfile
import time
import urllib.request
import zipfile

COMMIT = "07f806f999227108933c2e30515b26eecc1fda74"
SHA256 = "52f47072473375767d63ea5be9ffb96a3092124223fe5ce036834a299715014e"
ARCHIVE_HASHES = {
    "x64": SHA256,
    "arm64": "d55904f6048e351890bac7ac70d2380a8daa5e9ca2290a184ad4988c9f4d3432",
}


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
    extract_file(package, member, target)


def extract_file(package, member, target):
    """Retain identical mapped files and replace changed files atomically."""
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


def verify_archive(archive, architecture="x64"):
    with archive.open("rb") as data:
        actual = hashlib.file_digest(data, "sha256").hexdigest()
    if actual != ARCHIVE_HASHES[architecture]:
        raise ValueError(
            f"terminal archive checksum mismatch: {actual} ({archive.stat().st_size} bytes)"
        )


def download_archive(archive, architecture="x64"):
    url = f"https://update.code.visualstudio.com/commit:{COMMIT}/win32-{architecture}-archive/stable"
    temporary = archive.with_suffix(".partial")
    temporary.unlink(missing_ok=True)
    stalled = 0
    for attempt in range(1, 11):
        offset = temporary.stat().st_size if temporary.exists() else 0
        try:
            headers = {"Range": f"bytes={offset}-"} if offset else {}
            with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=120) as response:
                # Append only when the server confirms the requested offset;
                # otherwise the response restarts the whole archive.
                resumed = offset and response.status == 206 and (
                    response.headers.get("Content-Range", "").startswith(f"bytes {offset}-"))
                with temporary.open("ab" if resumed else "wb") as out:
                    shutil.copyfileobj(response, out)
                # Chunked reads end silently at an early close, leaving the
                # rest of the declared length unread.
                if response.length:
                    raise IncompleteRead(b"", response.length)
            # A CDN may return a truncated or incorrect body without an HTTP
            # error. Never promote it to the cache before checking the pin.
            verify_archive(temporary, architecture)
            temporary.replace(archive)
            return
        except ValueError as error:
            # Wrong bytes cannot be repaired by resuming; start over.
            temporary.unlink(missing_ok=True)
            stalled += 1
            failure = error
        except (OSError, IncompleteRead) as error:
            # Keep the received prefix; the next request resumes after it.
            progressed = temporary.exists() and temporary.stat().st_size > offset
            stalled = 0 if progressed else stalled + 1
            failure = error
        if stalled == 3 or attempt == 10:
            temporary.unlink(missing_ok=True)
            raise failure
        print(f"Terminal download attempt {attempt} failed: {failure}; "
              f"retrying from byte {temporary.stat().st_size if temporary.exists() else 0}", flush=True)
        time.sleep(min(attempt, 5))


def pe_machine(data):
    """Read a PE file's declared ABI before placing it beside native drivers."""
    if len(data) < 64 or data[:2] != b"MZ":
        raise ValueError("Native terminal file is not a PE executable")
    offset = int.from_bytes(data[60:64], "little")
    if offset + 6 > len(data) or data[offset:offset + 4] != b"PE\0\0":
        raise ValueError("Native terminal file has an invalid PE header")
    return int.from_bytes(data[offset + 4:offset + 6], "little")


def install_native_conpty(package, target, architecture):
    """Use the same verified ConPTY for ordinary Rust PTYs and visual journeys."""
    prefix = f"{COMMIT[:10]}/resources/app/node_modules.asar.unpacked/node-pty/"
    members = {member.filename.casefold(): member for member in package.infolist()}
    pairs = []
    for member in package.infolist():
        if member.filename.startswith(prefix) and member.filename.casefold().endswith("/conpty.dll"):
            companion = member.filename.rsplit("/", 1)[0] + "/OpenConsole.exe"
            if companion.casefold() in members:
                pairs.append((member, members[companion.casefold()]))
    if len(pairs) != 1:
        raise ValueError("Expected one ConPTY/OpenConsole pair in the pinned native host")
    expected_machine = {"x64": 0x8664, "arm64": 0xAA64}[architecture]
    directory = target.resolve() / "debug/deps"
    if not directory.resolve().is_relative_to(target.resolve()):
        raise ValueError(f"Native terminal driver directory escapes its target: {directory}")
    directory.mkdir(parents=True, exist_ok=True)
    installed = []
    for member, name in zip(pairs[0], ["conpty.dll", "OpenConsole.exe"]):
        data = package.read(member)
        if pe_machine(data) != expected_machine:
            raise ValueError(f"Pinned native terminal ABI differs from {architecture}: {member.filename}")
        path = directory / name
        if not path.resolve().is_relative_to(directory.resolve()):
            raise ValueError(f"Native terminal sidecar escapes its driver directory: {path}")
        extract_file(package, member, path)
        digest = hashlib.sha256(data).hexdigest()
        if hashlib.sha256(path.read_bytes()).hexdigest() != digest:
            raise ValueError(f"Installed native terminal differs from the verified archive: {path}")
        installed.append({"path": str(path), "member": member.filename, "sha256": digest})
    dynamic_load_checked = False
    if os.name == "nt" and pe_machine(Path(sys.executable).read_bytes()) == expected_machine:
        import ctypes
        library = ctypes.WinDLL(str(directory / "conpty.dll"))
        for symbol in ["CreatePseudoConsole", "ResizePseudoConsole", "ClosePseudoConsole"]:
            getattr(library, symbol)
        dynamic_load_checked = True
    identity = {"distribution_commit": COMMIT, "architecture": architecture,
                "archive_sha256": ARCHIVE_HASHES[architecture], "files": installed,
                "driver_directory": str(directory), "dynamic_load_checked": dynamic_load_checked}
    print("NATIVE_CONPTY_HOST=" + json.dumps(identity, sort_keys=True), flush=True)
    return identity


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("directory", type=Path)
    parser.add_argument("--native-target", type=Path)
    parser.add_argument("--native-arch", choices=ARCHIVE_HASHES)
    args = parser.parse_args()
    if args.native_arch is not None and args.native_target is None:
        parser.error("--native-arch requires --native-target")
    root = args.directory.resolve()
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
        if args.native_target is not None:
            architecture = args.native_arch or {
                "amd64": "x64", "x86_64": "x64", "arm64": "arm64", "aarch64": "arm64",
            }.get(platform.machine().lower())
            if architecture not in ARCHIVE_HASHES:
                raise ValueError(f"No pinned native ConPTY for {platform.machine()}")
            native_archive = archive if architecture == "x64" else root / f"host-{architecture}.zip"
            if not native_archive.exists():
                download_archive(native_archive, architecture)
            else:
                verify_archive(native_archive, architecture)
            with zipfile.ZipFile(native_archive) as package:
                install_native_conpty(package, args.native_target, architecture)
    print(f"Verified terminal host: {root / 'Code.exe'}")


if __name__ == "__main__":
    main()
