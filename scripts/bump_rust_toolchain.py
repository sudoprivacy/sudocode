#!/usr/bin/env python3
"""Move the pinned Rust toolchain to the latest stable release.

`rust-toolchain.toml` names the one Rust release that CI, release builds and
local acceptance compile with. This script is how that pin moves, both for
people and for `.github/workflows/rust-toolchain-bump.yml`. When the stable
channel is newer than the pin, it rewrites, together:

* `rust-toolchain.toml`;
* the `rust` base image of each local acceptance Dockerfile, as the registry's
  multi-platform index digest after checking that every Linux image in that
  index reports the new release;
* the acceptance contract pins in `.github/local-acceptance/checks.py` that
  hash those files.

Every value is computed from the committed `HEAD` plus these edits; the touched
files must not have uncommitted changes. Pins that are already stale at `HEAD`
stop the script rather than being refreshed along with the bump.

    python3 scripts/bump_rust_toolchain.py --dry-run   # show the planned diff
    python3 scripts/bump_rust_toolchain.py             # write it
    python3 scripts/bump_rust_toolchain.py --status    # how far the pin trails stable
"""

import argparse
import difflib
import hashlib
import importlib.util
import json
import os
import re
import subprocess
import sys
import tempfile
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TOOLCHAIN = "rust-toolchain.toml"
IMAGES = [".github/local-acceptance/Dockerfile", ".github/local-acceptance/Dockerfile.guard"]
CHECKS = ".github/local-acceptance/checks.py"
HELPER = ".github/scripts/local_acceptance.py"
# stage_profile_executor pins the recipes of these profiles; each hashes the
# acceptance Dockerfile and the checks.py code its dispatch reaches.
RECIPES = ["native-release", "native-context"]
CHANNEL_URL = "https://static.rust-lang.org/dist/channel-rust-stable.toml"
REGISTRY = "https://registry-1.docker.io"
REPOSITORY = "library/rust"
# Hosts that run the acceptance images: Intel and Apple silicon Docker.
REQUIRED_PLATFORMS = {("linux", "amd64"), ("linux", "arm64")}
INDEX_TYPES = ["application/vnd.oci.image.index.v1+json",
               "application/vnd.docker.distribution.manifest.list.v2+json"]
MANIFEST_TYPES = ["application/vnd.oci.image.manifest.v1+json",
                  "application/vnd.docker.distribution.manifest.v2+json"]
VERSION = re.compile(r"\d+\.\d+\.\d+")
# How many stable minor releases the pin may trail before it is overdue. Rust
# ships one every six weeks, so the pin is never more than about twelve weeks
# behind before the CI owners are told it is overdue.
ALLOWED_RELEASES_BEHIND = 1
FROM_RUST = re.compile(r"^FROM rust:(?P<tag>\S+?)@sha256:(?P<digest>[0-9a-f]{64})(?P<rest>.*)$", re.M)


class ImagePending(Exception):
    """The registry has not published the image for this release yet."""


def git(*args, data=None, env=None):
    result = subprocess.run(["git", *args], cwd=ROOT, input=data, env=env,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if result.returncode:
        raise RuntimeError(f"git {' '.join(args)}: {result.stderr.decode(errors='replace').strip()}")
    return result.stdout


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def version_key(version):
    return tuple(int(part) for part in version.split("."))


def fetch(url, headers=None):
    request = urllib.request.Request(url, headers={"User-Agent": "sudocode-rust-toolchain-bump", **(headers or {})})
    with urllib.request.urlopen(request, timeout=60) as response:
        return response.read(), response.headers


def pinned_channel(text):
    """The exact release in the [toolchain] table."""
    table = re.search(r"(?ms)^\[toolchain\]\s*$(.*?)(?=^\[|\Z)", text)
    match = table and re.search(r'(?m)^channel\s*=\s*"([^"]*)"\s*(?:#.*)?$', table.group(1))
    if not match or not VERSION.fullmatch(match.group(1)):
        raise RuntimeError(f"{TOOLCHAIN} must pin an exact release, such as channel = \"1.99.1\"")
    return match.group(1)


def latest_stable(url):
    """The release the stable channel manifest currently names."""
    manifest, _ = fetch(url)
    checksum, _ = fetch(url + ".sha256")
    if checksum.split()[0].decode() != sha256(manifest):
        raise RuntimeError(f"{url} does not match its published SHA256")
    text = manifest.decode("utf-8")
    try:
        import tomllib
        version = tomllib.loads(text)["pkg"]["rust"]["version"]
    except ImportError:
        version = re.search(r'(?ms)^\[pkg\.rust\]\s*$.*?^version = "([^"]+)"', text).group(1)
    match = VERSION.match(version)
    if not match:
        raise RuntimeError(f"Unexpected stable Rust version in {url}: {version!r}")
    return match.group()


class Registry:
    """Anonymous Docker Hub reads that verify content against its digest."""

    def __init__(self):
        body, _ = fetch(f"https://auth.docker.io/token?service=registry.docker.io&scope=repository:{REPOSITORY}:pull")
        self.token = json.loads(body)["token"]

    def get(self, kind, reference, accept):
        headers = {"Authorization": f"Bearer {self.token}", "Accept": ", ".join(accept)}
        body, response = fetch(f"{REGISTRY}/v2/{REPOSITORY}/{kind}/{reference}", headers)
        digest = "sha256:" + sha256(body)
        expected = reference if reference.startswith("sha256:") else response.get("Docker-Content-Digest")
        if expected and expected != digest:
            raise RuntimeError(f"Registry content for {kind}/{reference} hashes to {digest}, not {expected}")
        return digest, response.get("Content-Type", "").split(";")[0], body

    def index_digest(self, tag, version):
        """Digest of rust:<tag>, after checking each Linux image's RUST_VERSION."""
        try:
            digest, media_type, body = self.get("manifests", tag, INDEX_TYPES + MANIFEST_TYPES)
        except urllib.error.HTTPError as error:
            if error.code == 404:
                raise ImagePending(f"rust:{tag} is not published yet") from error
            raise
        if media_type not in INDEX_TYPES:
            raise RuntimeError(f"rust:{tag} is {media_type}, not a multi-platform index")
        platforms = set()
        for entry in json.loads(body)["manifests"]:
            platform = entry.get("platform", {})
            if platform.get("os") != "linux":
                continue  # attestation manifests use os "unknown"
            _, _, manifest = self.get("manifests", entry["digest"], MANIFEST_TYPES)
            _, _, config = self.get("blobs", json.loads(manifest)["config"]["digest"], ["application/json"])
            environment = json.loads(config)["config"].get("Env", [])
            if f"RUST_VERSION={version}" not in environment:
                raise RuntimeError(f"rust:{tag} {platform.get('architecture')} does not report RUST_VERSION={version}")
            platforms.add((platform["os"], platform.get("architecture")))
        missing = REQUIRED_PLATFORMS - platforms
        if missing:
            raise ImagePending(f"rust:{tag} has no image for {sorted(missing)} yet")
        return digest


def rebase_image(path, text, version, registry):
    """Point the file's single rust base image at <version>, keeping its variant."""
    matches = list(FROM_RUST.finditer(text))
    if len(matches) != 1:
        raise RuntimeError(f"{path} must have exactly one digest-pinned 'FROM rust:<tag>@sha256:' line")
    match = matches[0]
    variant = re.fullmatch(r"\d+\.\d+(?:\.\d+)?(?P<variant>(?:-[\w.-]+)?)", match.group("tag"))
    if not variant:
        raise RuntimeError(f"{path} base tag rust:{match.group('tag')} does not start with a Rust version")
    tag = version + variant.group("variant")
    digest = registry.index_digest(tag, version)
    line = f"FROM rust:{tag}@{digest}{match.group('rest')}"
    return text[:match.start()] + line + text[match.end():], {"path": path, "from": match.group(0), "to": line}


def snapshot(contents):
    """A dangling commit of HEAD plus these blobs, for helpers that read Git."""
    with tempfile.TemporaryDirectory() as temporary:
        env = {**os.environ, "GIT_INDEX_FILE": str(Path(temporary) / "index"),
               "GIT_AUTHOR_NAME": "rust-toolchain-bump", "GIT_AUTHOR_EMAIL": "rust-toolchain-bump@invalid",
               "GIT_COMMITTER_NAME": "rust-toolchain-bump", "GIT_COMMITTER_EMAIL": "rust-toolchain-bump@invalid"}
        git("read-tree", "HEAD", env=env)
        for path, data in contents.items():
            blob = git("hash-object", "-w", "--no-filters", "--stdin", data=data).decode().strip()
            mode = git("ls-tree", "HEAD", "--", path).decode().split(" ", 1)[0] or "100644"
            git("update-index", "--add", "--cacheinfo", f"{mode},{blob},{path}", env=env)
        tree = git("write-tree", env=env).decode().strip()
        return git("commit-tree", tree, "-p", "HEAD", "-m", "Rust toolchain bump preview", env=env).decode().strip()


def load_helper():
    """The committed acceptance helper, which computes the recipe pins."""
    with tempfile.TemporaryDirectory() as temporary:
        path = Path(temporary) / "local_acceptance.py"
        path.write_bytes(git("show", f"HEAD:{HELPER}"))
        spec = importlib.util.spec_from_file_location("bump_acceptance_helper", path)
        helper = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(helper)
    helper.ROOT = ROOT
    return helper


RECIPE_PIN = re.compile(r"expected = \{'native-release': '([0-9a-f]{64})', 'native-context': '([0-9a-f]{64})'\}")


def recipe_pins(source):
    match = RECIPE_PIN.search(source)
    if not match:
        raise RuntimeError(f"{CHECKS} no longer has the recipe pin this script maintains; update the script")
    return dict(zip(RECIPES, match.groups())), match


def file_pins(source, path):
    """checks.py entries that pin path; guard image names are relative to its directory."""
    directory = str(Path(CHECKS).parent) + "/"
    keys = {path, path[len(directory):]} if path.startswith(directory) else {path}
    return [match for match in re.finditer(r'"([^"\s]+)": "([0-9a-f]{64})"', source) if match.group(1) in keys]


def repin(original, edited, helper):
    """Return checks.py with every pin over the edited files updated."""
    source = original[CHECKS].decode("utf-8")
    pins, _ = recipe_pins(source)
    if helper.reachable_profile_commands_hashes("HEAD", RECIPES) != pins:
        raise RuntimeError(f"{CHECKS} recipe pins are already stale at HEAD; repin them before bumping")
    changes = []
    for path, data in edited.items():
        for match in file_pins(source, path):
            if match.group(2) != sha256(original[path]):
                raise RuntimeError(f"{CHECKS} pin for {path} is already stale at HEAD; repin it before bumping")
            source = source.replace(match.group(0), f'"{match.group(1)}": "{sha256(data)}"')
            changes.append(f"{path} file pin")
    # A recipe hashes the checks.py code it reaches; repeat until it is stable.
    for _ in range(5):
        observed = helper.reachable_profile_commands_hashes(
            snapshot({**edited, CHECKS: source.encode("utf-8")}), RECIPES)
        current, match = recipe_pins(source)
        if observed == current:
            break
        source = source.replace(match.group(0), "expected = {" + ", ".join(
            f"'{name}': '{observed[name]}'" for name in RECIPES) + "}")
        changes.extend(f"{name} recipe pin" for name in RECIPES if observed[name] != current[name])
    else:
        raise RuntimeError(f"{CHECKS} recipe pins did not reach a fixed point")
    return source.encode("utf-8"), sorted(set(changes))


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n", 1)[0])
    parser.add_argument("--dry-run", action="store_true", help="print the planned diff without writing files")
    parser.add_argument("--version", help="target release instead of the stable channel's, e.g. 1.100.1")
    parser.add_argument("--channel-url", default=CHANNEL_URL)
    parser.add_argument("--github-output", type=Path, help="append changed/previous/version for a workflow")
    parser.add_argument("--summary", type=Path, help="write a Markdown summary of the change")
    parser.add_argument("--status", action="store_true",
                        help="report how far the pin trails stable (current, available or overdue); writes nothing")
    args = parser.parse_args()

    def report(**values):
        if args.github_output:
            with args.github_output.open("a", encoding="utf-8") as stream:
                stream.writelines(f"{key}={value}\n" for key, value in values.items())

    if args.status:
        pinned = pinned_channel((ROOT / TOOLCHAIN).read_text(encoding="utf-8"))
        stable = latest_stable(args.channel_url)
        (pin_major, pin_minor, _), (major, minor, _) = version_key(pinned), version_key(stable)
        behind = max(0, minor - pin_minor) if major == pin_major else ALLOWED_RELEASES_BEHIND + 1
        state = ("current" if version_key(stable) <= version_key(pinned)
                 else "overdue" if behind > ALLOWED_RELEASES_BEHIND else "available")
        print(f"Rust {pinned} is pinned; stable is {stable}: {state} ({behind} minor release(s) behind, "
              f"{ALLOWED_RELEASES_BEHIND} allowed).")
        report(pinned=pinned, stable=stable, behind=behind, allowed=ALLOWED_RELEASES_BEHIND, state=state)
        return

    touched = [TOOLCHAIN, *IMAGES, CHECKS]
    if git("status", "--porcelain", "--", *touched).strip():
        raise RuntimeError(f"Commit or stash local changes to {', '.join(touched)} first")
    original = {path: git("show", f"HEAD:{path}") for path in touched}
    current = pinned_channel(original[TOOLCHAIN].decode("utf-8"))
    target = args.version or latest_stable(args.channel_url)
    if not VERSION.fullmatch(target):
        raise RuntimeError(f"Target must be an exact release such as 1.100.1, not {target!r}")
    if version_key(target) <= version_key(current):
        print(f"Rust {current} is pinned; the target {target} is not newer. Nothing to do.")
        report(changed="false", previous=current, version=current)
        return

    print(f"Moving the Rust pin from {current} to {target}.")
    edited = {TOOLCHAIN: re.sub(r'(?m)^(channel\s*=\s*")[^"]*(")', rf"\g<1>{target}\g<2>",
                                original[TOOLCHAIN].decode("utf-8"), count=1).encode("utf-8")}
    registry = Registry()
    images = []
    try:
        for path in IMAGES:
            text, image = rebase_image(path, original[path].decode("utf-8"), target, registry)
            edited[path] = text.encode("utf-8")
            images.append(image)
    except ImagePending as pending:
        print(f"{pending}; the pin stays at {current}. Run again once Docker Hub publishes it.")
        report(changed="false", previous=current, version=current, pending=target)
        return
    edited[CHECKS], pins = repin(original, edited, load_helper())

    for path, data in edited.items():
        sys.stdout.writelines(difflib.unified_diff(
            original[path].decode("utf-8").splitlines(keepends=True), data.decode("utf-8").splitlines(keepends=True),
            f"a/{path}", f"b/{path}"))
    if args.summary:
        args.summary.write_text("\n".join([
            f"Moves the pinned Rust toolchain from {current} to {target}, the current stable release.",
            "",
            *[f"- `{image['path']}`: `{image['to']}`" for image in images],
            f"- `{CHECKS}`: " + ", ".join(pins),
            "",
            "Each image's linux/amd64 and linux/arm64 entries report `RUST_VERSION=" + target + "`.",
        ]) + "\n", encoding="utf-8")
    if args.dry_run:
        print("Dry run: no files were written.")
    else:
        for path, data in edited.items():
            (ROOT / path).write_bytes(data)
    report(changed="false" if args.dry_run else "true", previous=current, version=target)


if __name__ == "__main__":
    try:
        main()
    except (RuntimeError, urllib.error.URLError) as error:
        sys.exit(f"bump_rust_toolchain: {error}")
