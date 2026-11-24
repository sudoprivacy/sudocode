"""Verify an image's installed Rust for a lookup before numeric installation."""

import hashlib
import os
import re
import subprocess
from pathlib import Path
from urllib.request import urlopen

import tomllib


def verified_release(channel):
    url = f"https://static.rust-lang.org/dist/channel-rust-{channel}.toml"
    with urlopen(url + ".sha256", timeout=4) as response:
        checksum = response.read(512).decode("ascii").split()[0]
    with urlopen(url, timeout=4) as response:
        manifest = response.read(2 * 1024 * 1024)
    if (
        not re.fullmatch(r"[0-9a-f]{64}", checksum)
        or hashlib.sha256(manifest).hexdigest() != checksum
    ):
        raise ValueError("Unexpected official Rust manifest checksum")
    release = tomllib.loads(manifest.decode("utf-8"))["pkg"]["rustc"]
    if not release["version"].startswith(channel + " ("):
        raise ValueError("Official Rust manifest does not match the pinned version")
    return release


def lookup_inputs(environ):
    if any(
        environ.get(key)
        for key in ("RUSTUP_TOOLCHAIN", "RUSTUP_DIST_SERVER", "RUSTUP_DIST_ROOT")
    ):
        raise ValueError(
            "Explicit toolchain configuration retains the ordinary install"
        )
    workspace = Path(environ["GITHUB_WORKSPACE"])
    config = tomllib.loads(
        (workspace / "rust-toolchain.toml").read_text(encoding="utf-8")
    )["toolchain"]
    channel = config["channel"]
    if (
        not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", channel)
        or config.get("profile", "minimal") != "minimal"
    ):
        raise ValueError("Lookup requires a numeric Rust pin with the minimal profile")

    environment = {**environ, "RUSTUP_AUTO_INSTALL": "0"}
    compiler = subprocess.check_output(
        ["rustup", "run", "stable", "rustc", "-vV"],
        env=environment,
        text=True,
        stderr=subprocess.DEVNULL,
        timeout=5,
    )
    identity = dict(
        line.split(": ", 1) for line in compiler.splitlines() if ": " in line
    )
    architecture = {"X64": "x86_64", "ARM64": "aarch64"}[environ["RUNNER_ARCH"]]
    suffix = {
        "Linux": "unknown-linux-gnu",
        "Windows": "pc-windows-msvc",
        "macOS": "apple-darwin",
    }
    host = architecture + "-" + suffix[environ["RUNNER_OS"]]
    if identity.get("release") != channel or identity.get("host") != host:
        raise ValueError("The image compiler differs from the pinned host release")
    installed = subprocess.check_output(
        ["rustup", "component", "list", "--toolchain", "stable", "--installed"],
        env=environment,
        text=True,
        stderr=subprocess.DEVNULL,
        timeout=5,
    ).splitlines()
    components = {"rustc", "cargo", "rust-std", *config.get("components", [])}
    required = {f"{component}-{host}" for component in components}
    required.update(f"rust-std-{target}" for target in config.get("targets", []))
    if not required.issubset(installed):
        raise ValueError("The image is missing a pinned Rust component or target")
    if identity.get("commit-hash") != verified_release(channel)["git_commit_hash"]:
        raise ValueError(
            "The image Rust commit differs from the official pinned release"
        )

    cargo_home = environ.get("CARGO_HOME")
    if not cargo_home:
        cargo_home = (
            environ["USERPROFILE"] + "\\.cargo"
            if environ["RUNNER_OS"] == "Windows"
            else environ["HOME"] + "/.cargo"
        )
    helper = (
        workspace / ".github/actions/rust-toolchain/preinstalled-cache-command.mjs"
    ).as_posix()
    if any(char in cargo_home + helper for char in '\r\n"'):
        raise ValueError("Unexpected cache output path")
    return {
        "cargo-home": cargo_home,
        "dependency-cache-command": f'node "{helper}" {host} {{0}}',
    }


def main():
    try:
        values = lookup_inputs(os.environ)
    except (OSError, ValueError, KeyError, IndexError, subprocess.SubprocessError):
        print("Preinstalled Rust lookup unavailable, using the original installation")
        return
    with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as stream:
        stream.writelines(f"{key}={value}\n" for key, value in values.items())
    print("Verified the image Rust commit and components for a cache lookup")


if __name__ == "__main__":
    main()
