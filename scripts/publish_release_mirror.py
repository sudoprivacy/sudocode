#!/usr/bin/env python3
"""Stage immutable COS releases, then promote one verified version pointer."""

import argparse
import hashlib
import re
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def remote_digest(url):
    # Bypass stale intermediary caches when verifying an upload or retry.
    request = urllib.request.Request(
        f"{url}?verify={time.time_ns()}", headers={"Cache-Control": "no-cache"}
    )
    try:
        with urllib.request.urlopen(request, timeout=120) as response:
            return hashlib.file_digest(response, "sha256").hexdigest()
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise


def payload(dist, installer, version):
    checksums = dist / "SHA256SUMS.txt"
    files = {}
    for line in checksums.read_text().splitlines():
        match = re.fullmatch(r"([a-f0-9]{64})  ([A-Za-z0-9_.-]+)", line)
        if not match:
            raise ValueError(f"invalid checksum entry: {line!r}")
        expected, name = match.groups()
        if name in files or not name.startswith("scode"):
            raise ValueError(f"invalid or duplicate artifact: {name}")
        path = dist / name
        if digest(path) != expected:
            raise ValueError(f"local checksum mismatch: {name}")
        files[name] = path
    required = {
        f"scode-{platform}.tar.gz"
        for platform in ("linux-x64", "linux-arm64", "macos-x64", "macos-arm64")
    } | {f"scode-windows-{arch}.zip" for arch in ("x64", "arm64")}
    required.add(f"scode_{version[1:]}_amd64.deb")
    if set(files) != required:
        raise ValueError(f"release artifact set differs: {set(files) ^ required}")
    return {**files, "SHA256SUMS.txt": checksums, "install.sh": installer}


def upload(path, key):
    subprocess.run(["coscmd", "upload", str(path), key], check=True)


def verify(url, expected):
    for attempt in range(3):
        if remote_digest(url) == expected:
            return
        if attempt < 2:
            time.sleep(1 << attempt)
    raise ValueError(f"mirror readback checksum mismatch: {url}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("phase", choices=("stage", "promote"))
    parser.add_argument("--version", required=True)
    parser.add_argument("--dist", type=Path, default=Path("dist"))
    parser.add_argument("--installer", type=Path, default=Path("install.sh"))
    parser.add_argument("--prefix", default="sudocode/release")
    parser.add_argument(
        "--public-base",
        default="https://sudowork-release-1309794936.cos.ap-beijing.myqcloud.com",
    )
    args = parser.parse_args()
    if not re.fullmatch(r"v\d+\.\d+\.\d+", args.version):
        parser.error("only stable vX.Y.Z releases can be mirrored")
    files = payload(args.dist, args.installer, args.version)
    base = args.public_base.rstrip("/")
    prefix = args.prefix.strip("/")
    for name, path in files.items():
        key = f"{prefix}/{args.version}/{name}"
        expected = digest(path)
        existing = remote_digest(f"{base}/{key}")
        if existing is not None and existing != expected:
            raise ValueError(f"refusing to replace immutable object: {key}")
        if existing is None:
            if args.phase == "promote":
                raise ValueError(f"release is not fully staged: {key}")
            upload(path, key)
            verify(f"{base}/{key}", expected)
        print(f"verified {key}", flush=True)

    if args.phase == "promote":
        # The workflow serializes publishers. A retry of an older release may
        # verify its immutable assets, but must not roll latest backward.
        pointer = f"{prefix}/latest/version.txt"
        try:
            request = urllib.request.Request(
                f"{base}/{pointer}?verify={time.time_ns()}",
                headers={"Cache-Control": "no-cache"},
            )
            with urllib.request.urlopen(request, timeout=30) as response:
                previous = response.read(128).decode().strip()
        except urllib.error.HTTPError as error:
            if error.code != 404:
                raise
            previous = ""
        if previous:
            if not re.fullmatch(r"v\d+\.\d+\.\d+", previous):
                raise ValueError("invalid existing mirror version pointer")
            if tuple(map(int, previous[1:].split("."))) > tuple(
                map(int, args.version[1:].split("."))
            ):
                print(f"latest remains at newer {previous}")
                return
        # Keep the documented curl URL usable. This installer understands both
        # the old flat layout and the pointer; archives are never overwritten.
        upload(args.installer, f"{prefix}/latest/install.sh")
        verify(f"{base}/{prefix}/latest/install.sh", digest(args.installer))
        with tempfile.TemporaryDirectory(prefix="scode-mirror-") as directory:
            path = Path(directory) / "version.txt"
            path.write_text(args.version + "\n")
            upload(path, pointer)
            verify(f"{base}/{pointer}", digest(path))
        print(f"promoted {args.version}")


if __name__ == "__main__":
    main()
