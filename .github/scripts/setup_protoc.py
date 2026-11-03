"""Install the shared protoc required by all-feature checks and cohost builds."""

from io import BytesIO
import json
import os
from pathlib import Path, PurePosixPath
import platform
import subprocess
from urllib.request import urlopen
from zipfile import ZipFile


# raft-proto's protobuf-build accepts only major version 3 and has no bundled
# Apple Silicon compiler. Keep every CI consumer on the same supported version.
VERSION = "3.20.2"
ARCHIVES = {
    ("Linux", "x86_64"): "linux-x86_64",
    ("Linux", "aarch64"): "linux-aarch_64",
    ("Darwin", "x86_64"): "osx-x86_64",
    ("Darwin", "arm64"): "osx-aarch_64",
    ("Windows", "amd64"): "win64",
}


def main():
    """Publish the verified compiler to subsequent steps in the current job."""
    host = (platform.system(), platform.machine().lower())
    archive = ARCHIVES[host]
    directory = Path(os.environ["RUNNER_TEMP"]) / f"protoc-{VERSION}-{archive}"
    url = (
        "https://github.com/protocolbuffers/protobuf/releases/download/"
        f"v{VERSION}/protoc-{VERSION}-{archive}.zip"
    )
    with urlopen(url, timeout=60) as response:
        with ZipFile(BytesIO(response.read())) as package:
            for name in package.namelist():
                path = PurePosixPath(name)
                if path.is_absolute() or ".." in path.parts:
                    raise ValueError(f"Invalid protoc archive path: {name}")
            package.extractall(directory)

    binary = directory / "bin" / ("protoc.exe" if host[0] == "Windows" else "protoc")
    if host[0] != "Windows":
        binary.chmod(0o755)
    version = subprocess.check_output([str(binary), "--version"], text=True).strip()
    if version != f"libprotoc {VERSION}":
        raise ValueError(f"Unexpected protoc version: {version}")

    for name, value in (
        ("GITHUB_PATH", str(binary.parent)),
        ("GITHUB_ENV", f"PROTOC={binary}"),
    ):
        with Path(os.environ[name]).open("a", encoding="utf-8") as stream:
            stream.write(value + "\n")
    print(json.dumps({"version": version, "host": host, "binary": str(binary)}))


if __name__ == "__main__":
    main()
