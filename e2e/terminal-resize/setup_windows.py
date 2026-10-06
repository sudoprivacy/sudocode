"""Install the checksum-pinned Electron/ConPTY/xterm runtime used by the PTY test."""

import hashlib
from pathlib import Path
import shutil
import sys
import urllib.request
import zipfile

COMMIT = "07f806f999227108933c2e30515b26eecc1fda74"
SHA256 = "52f47072473375767d63ea5be9ffb96a3092124223fe5ce036834a299715014e"


def main():
    root = Path(sys.argv[1]).resolve()
    root.mkdir(parents=True, exist_ok=True)
    archive = root / "host.zip"
    if not archive.exists():
        url = f"https://update.code.visualstudio.com/commit:{COMMIT}/win32-x64-archive/stable"
        temporary = archive.with_suffix(".partial")
        with urllib.request.urlopen(url, timeout=120) as response, temporary.open("wb") as out:
            shutil.copyfileobj(response, out)
        temporary.replace(archive)
    with archive.open("rb") as data:
        actual = hashlib.file_digest(data, "sha256").hexdigest()
    if actual != SHA256:
        raise RuntimeError(f"terminal archive checksum mismatch: {actual}")
    version = COMMIT[:10]
    with zipfile.ZipFile(archive) as package:
        # The Node mode needs no editor extensions, language servers or updater.
        # Extract from the same verified distribution, preserving native ABI parity.
        for name in package.namelist():
            if name == "Code.exe" or (
                name.startswith(f"{version}/") and name.count("/") == 1 and not name.endswith("/")
            ) or name in {
                f"{version}/resources/app/node_modules.asar",
                f"{version}/resources/app/product.json",
                f"{version}/resources/app/package.json",
            } or name.startswith(f"{version}/resources/app/node_modules.asar.unpacked/node-pty/"):
                package.extract(name, root)
    print(f"Verified terminal host: {root / 'Code.exe'}")


if __name__ == "__main__":
    main()
