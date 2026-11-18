"""Transport accepted queue compiler inputs to the sole main cache publisher.

The producer's exact image/compiler key is retained. This module never runs an
imported compiler artifact, signs acceptance, or reuses a previous test result.
"""

import argparse
import copy
from decimal import Decimal
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import stat
import subprocess
import tarfile
import tempfile
import urllib.request
import zipfile

import vfs_search_cache as cache


REPOSITORY = "sudoprivacy/sudocode"
API = "https://api.github.com/repos/" + REPOSITORY
WORKFLOW = ".github/workflows/rust-ci.yml"
JOB = "nexus co-host journeys (real daemon)"
EXPORT_STEP = "Export the successful queue compiler inputs"
MANIFEST_LIMIT = 2 * 1024 * 1024
ARCHIVE_LIMIT = 12 * 1024**3
MEMBER_LIMIT = 500_000
SOURCES = [WORKFLOW, ".github/workflows/ci-vfs-search-cache.yml",
           ".github/scripts/vfs_search_cache.py", ".github/scripts/vfs_search_publication.py",
           ".github/actions/vfs-search-cache/action.yml",
           ".github/actions/vfs-search-cache/functional.toml", ".github/ci/cohost-plugin-crypto.toml",
           "rust/Cargo.lock"]


def sha(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def write(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def source_hashes(root):
    if any((root / name).is_symlink() or not (root / name).is_file() for name in SOURCES):
        raise RuntimeError("Compiler publication requires regular reviewed source inputs")
    return {name: sha(root / name) for name in SOURCES}


def positive(value):
    if type(value) is not int or value <= 0:
        raise RuntimeError("Invalid workflow/attempt/artifact identity")
    return value


def scrub_credentials():
    for name in list(os.environ):
        if name.upper() in {"GH_TOKEN", "GITHUB_TOKEN"}:
            del os.environ[name]


class GitHub:
    """Use the existing exact-origin, unredirected API credential contract."""

    def __init__(self, root):
        spec = importlib.util.spec_from_file_location("publication_requests", root / ".github/local-acceptance/checks.py")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        self.request = module.github_api_request(API, "application/vnd.github+json")
        scrub_credentials()  # No subsequent Git/tool child inherits API authority.

    def read(self, suffix, limit=MANIFEST_LIMIT):
        if not suffix.startswith("/") or ".." in suffix or "#" in suffix:
            raise RuntimeError("Invalid publication API path")
        request = copy.copy(self.request)
        request.full_url = API + suffix
        with urllib.request.urlopen(request, timeout=30) as response:
            payload = response.read(limit + 1)
        if len(payload) > limit:
            raise RuntimeError("Publication API/artifact response exceeds its bound")
        return payload

    def json(self, suffix):
        return json.loads(self.read(suffix))

    def pages(self, suffix, key):
        rows = []
        separator = "&" if "?" in suffix else "?"
        for page in range(1, 11):
            data = self.json(f"{suffix}{separator}per_page=100&page={page}")
            items = data[key]
            if not isinstance(items, list) or data["total_count"] > 1000:
                raise RuntimeError("Publication API inventory exceeds its supported bound")
            rows.extend(items)
            if len(items) < 100:
                if len(rows) != data["total_count"]:
                    raise RuntimeError("Publication API inventory changed during pagination")
                return rows
        raise RuntimeError("Publication API inventory was truncated")


def producer_run(api, head):
    """Newest current-attempt run owns the head, including a failed rerun."""
    runs = api.pages(f"/actions/workflows/rust-ci.yml/runs?head_sha={head}&event=merge_group", "workflow_runs")
    if not runs:
        return None
    run = api.json(f"/actions/runs/{max(positive(row['id']) for row in runs)}")
    if (run.get("head_sha") != head or run.get("event") != "merge_group"
            or run.get("path", "").split("@")[0] != WORKFLOW
            or run.get("repository", {}).get("full_name") != REPOSITORY
            or run.get("head_repository", {}).get("full_name") != REPOSITORY
            or run.get("status") != "completed" or run.get("conclusion") != "success"):
        raise RuntimeError("Only the successful current attempt of this accepted queue head can publish")
    positive(run["run_attempt"])
    return run


def artifact_name(kind, run):
    return f"vfs-search-{kind}-{positive(run['id'])}-{positive(run['run_attempt'])}"


def artifact(rows, kind, run, head):
    matching = [item for item in rows if item.get("name") == artifact_name(kind, run)]
    if len(matching) != 1:
        raise RuntimeError("Expected exactly one current-attempt compiler publication artifact")
    item = matching[0]
    owner = item.get("workflow_run", {})
    if (item.get("expired") is not False or owner.get("id") != run["id"] or owner.get("head_sha") != head
            or owner.get("repository_id") != run["repository"]["id"]
            or owner.get("head_repository_id") != run["repository"]["id"]
            or not re.fullmatch(r"sha256:[a-f0-9]{64}", item.get("digest", ""))):
        raise RuntimeError("Artifact provenance/digest does not match the accepted queue producer")
    positive(item["id"])
    return item


def read_manifest(api, item):
    payload = api.read(f"/actions/artifacts/{item['id']}/zip")
    if "sha256:" + hashlib.sha256(payload).hexdigest() != item["digest"]:
        raise RuntimeError("Compiler manifest artifact digest differs from GitHub")
    with zipfile.ZipFile(io.BytesIO(payload)) as archive:
        members = archive.infolist()
        if len(members) != 1 or members[0].filename != "manifest.json" or members[0].file_size > MANIFEST_LIMIT:
            raise RuntimeError("Compiler manifest artifact must contain one bounded manifest.json")
        return json.loads(archive.read(members[0]))


def validate_manifest(root, document, run, head):
    if not re.fullmatch(r"[a-f0-9]{40}", head):
        raise RuntimeError("Compiler publication requires an immutable source head")
    positive(document["run_id"])
    positive(document["run_attempt"])
    inputs = document["inputs"]
    observed = cache.compiler_inputs(root)
    # This is transport across images, not compiler/image equivalence. The key
    # below still includes the ORIGINAL ImageVersion; no imported program runs.
    image = inputs["environment"].get("ImageVersion", "")
    if not re.fullmatch(r"\d{8}\.\d+\.\d+", image):
        raise RuntimeError("Compiler producer must identify its original hosted image")
    observed["environment"]["ImageVersion"] = image
    key = f"vfs-search-v2-{cache.compiler_context(inputs)}-{cache.pin(root)}"
    expected = {"version": 1, "repository": REPOSITORY, "head": head,
                "run_id": run["id"], "run_attempt": run["run_attempt"], "job": "cohost-live"}
    if (any(document.get(name) != value for name, value in expected.items())
            or document.get("sources") != source_hashes(root)
            or inputs != observed or document.get("key") != key):
        raise RuntimeError("Compiler manifest source, runtime context or original key differs")
    evidence = document["journey"]
    context = evidence["context"]
    command = evidence["command"]
    log = evidence["log"].encode("utf-8")
    wanted = ["cargo", "test", "--locked", "--manifest-path", str(root / ".native-vfs/Cargo.toml"),
              "-p", "nexus-cluster", "--all-features", "--test", "user_runtime", cache.CASE, "--", "--nocapture"]
    if (context.get("source_revision") != inputs["pin"] or context.get("recipe_sha256") != inputs["recipe"]
            or context.get("functional_profile") != inputs["functional_profile"]
            or context.get("test_required_now") is not True
            or not re.fullmatch(r"[a-f0-9]{64}", context.get("current_runtime_sha256") or "")
            or command.get("command") != wanted or command.get("exit_code") != 0
            or command.get("log_sha256") != hashlib.sha256(log).hexdigest()
            or cache.CASE not in evidence["log"] or "test result: ok. 1 passed; 0 failed;" not in evidence["log"]):
        raise RuntimeError("Compiler export lacks the original successful current-runtime Cargo journey")
    description = document.get("archive")
    if description is not None:
        if (set(description) != {"name", "sha256", "size", "members", "bytes"}
                or description["name"] != "compiler.tar.gz" or not re.fullmatch(r"[a-f0-9]{64}", description["sha256"])
                or not 0 < positive(description["size"]) <= ARCHIVE_LIMIT
                or not 0 < positive(description["members"]) <= MEMBER_LIMIT
                or not 0 < positive(description["bytes"]) <= ARCHIVE_LIMIT):
            raise RuntimeError("Invalid compiler archive description")
    return key


def export(root, output, journey, include_archive):
    scrub_credentials()
    output.mkdir(parents=True, exist_ok=False)
    source = root / ".native-vfs"
    if output == source or source in output.parents:
        raise RuntimeError("Compiler export output must stay outside its source/compiler inputs")
    key, inputs = cache.cache_key(root)
    cache.verify_identity(root, source, inputs["pin"], inputs["pin"])
    cache.verify(source, inputs["pin"])
    commands = json.loads((journey / "commands.json").read_text(encoding="utf-8"))
    if len(commands) != 1:
        raise RuntimeError("Export requires the original single current-runtime test command")
    document = {"version": 1, "repository": os.environ.get("GITHUB_REPOSITORY"), "head": os.environ.get("GITHUB_SHA"),
                "run_id": int(os.environ["GITHUB_RUN_ID"]), "run_attempt": int(os.environ["GITHUB_RUN_ATTEMPT"]),
                "job": os.environ.get("GITHUB_JOB"), "key": key, "inputs": inputs, "sources": source_hashes(root),
                "journey": {"context": json.loads((journey / "context.json").read_text(encoding="utf-8")),
                            "command": commands[0], "log": (journey / "00.log").read_bytes().decode("utf-8")},
                "archive": None}
    run = {"id": document["run_id"], "run_attempt": document["run_attempt"]}
    validate_manifest(root, document, run, document["head"])
    if include_archive:
        destination = output / "data"
        destination.mkdir()
        path = destination / "compiler.tar.gz"
        count, total = 0, 0
        # Stream one low-cost compression pass instead of materializing a second
        # multi-GiB uncompressed target alongside the actual consumer build.
        with tarfile.open(path, "w:gz", format=tarfile.PAX_FORMAT, compresslevel=1) as archive:
            for directory, children, files in os.walk(source, followlinks=False):
                for name in sorted(children + files):
                    current = Path(directory) / name
                    info = current.lstat()
                    if not (stat.S_ISDIR(info.st_mode) or stat.S_ISREG(info.st_mode)):
                        raise RuntimeError("Compiler publication refuses links and special files")
                    relative = current.relative_to(source).as_posix()
                    member = tarfile.TarInfo(relative)
                    member.type = tarfile.DIRTYPE if current.is_dir() else tarfile.REGTYPE
                    member.mode = stat.S_IMODE(info.st_mode) & 0o777
                    member.pax_headers["mtime"] = str(Decimal(info.st_mtime_ns) / 1_000_000_000)
                    member.size = info.st_size if current.is_file() else 0
                    total += member.size
                    count += 1
                    if total > ARCHIVE_LIMIT or count > MEMBER_LIMIT:
                        raise RuntimeError("Compiler export exceeds its supported size/inventory")
                    if current.is_file():
                        with current.open("rb") as stream:
                            archive.addfile(member, stream)
                    else:
                        archive.addfile(member)
        document["archive"] = {"name": path.name, "sha256": sha(path), "size": path.stat().st_size,
                               "members": count, "bytes": total}
    validate_manifest(root, document, run, document["head"])
    metadata = output / "identity"
    metadata.mkdir()
    write(metadata / "manifest.json", document)
    if (metadata / "manifest.json").stat().st_size > MANIFEST_LIMIT:
        raise RuntimeError("Compiler publication manifest exceeds its supported bound")


def select(root, output, api, head):
    output.mkdir(parents=True, exist_ok=False)
    run = producer_run(api, head)
    result = {"available": False, "reason": "no original queue compiler export", "head": head}
    if run:
        jobs = api.pages(f"/actions/runs/{run['id']}/attempts/{run['run_attempt']}/jobs", "jobs")
        jobs = [job for job in jobs if job.get("name") == JOB]
        if len(jobs) > 1:
            raise RuntimeError("Ambiguous current-attempt compiler job")
        rows = api.pages(f"/actions/runs/{run['id']}/artifacts", "artifacts")
        names = {item.get("name") for item in rows}
        identity_present = artifact_name("identity", run) in names
        if artifact_name("compiler", run) in names and not identity_present:
            raise RuntimeError("Compiler data has no current-attempt producer identity")
        if identity_present:
            if (len(jobs) != 1 or jobs[0].get("conclusion") != "success" or jobs[0].get("status") != "completed"
                    or jobs[0].get("run_id") != run["id"] or jobs[0].get("run_attempt") != run["run_attempt"]
                    or jobs[0].get("head_sha") != head):
                raise RuntimeError("Compiler artifact requires its exact completed current-attempt job")
            steps = jobs[0]["steps"]
            for name in ["Search authorization and restart under the confined co-host owner",
                         "The driver-ai co-host still resolves", EXPORT_STEP, "Upload queue compiler identity"]:
                if len([step for step in steps if step.get("name") == name and step.get("conclusion") == "success"]) != 1:
                    raise RuntimeError("Original compiler producer steps did not all pass")
            item = artifact(rows, "identity", run, head)
            document = read_manifest(api, item)
            key = validate_manifest(root, document, run, head)
            blob = artifact(rows, "compiler", run, head) if document["archive"] else None
            if blob and len([step for step in steps if step.get("name") == "Upload queue compiler data"
                             and step.get("conclusion") == "success"]) != 1:
                raise RuntimeError("Original compiler data upload did not pass")
            write(output / "manifest.json", document)
            result = {"available": True, "key": key, "run_id": run["id"], "run_attempt": run["run_attempt"],
                      "archive": bool(blob), "artifact_id": blob["id"] if blob else 0,
                      "head": head, "manifest_artifact": item, "data_artifact": blob}
    write(output / "selection.json", result)
    return result


def import_archive(root, output, data, api, head):
    selection = json.loads((output / "selection.json").read_text(encoding="utf-8"))
    document = json.loads((output / "manifest.json").read_text(encoding="utf-8"))
    run = producer_run(api, head)
    if (not selection.get("available") or not selection.get("archive") or run is None
            or selection.get("head") != head or selection.get("run_id") != run["id"]
            or selection.get("run_attempt") != run["run_attempt"]):
        raise RuntimeError("Original successful compiler attempt is no longer current")
    key = validate_manifest(root, document, run, head)
    path = data / "compiler.tar.gz"
    description = document["archive"]
    if (selection["key"] != key or path.is_symlink() or not path.is_file()
            or path.stat().st_size != description["size"] or sha(path) != description["sha256"]):
        raise RuntimeError("Downloaded compiler archive differs from its original producer")
    target = root / ".native-vfs"
    if target.exists() or target.is_symlink():
        raise RuntimeError("Compiler import requires an absent private target; nothing was replaced")
    with tempfile.TemporaryDirectory(prefix="vfs-compiler-import-", dir=root) as temporary:
        staged = Path(temporary) / "checkout"
        staged.mkdir()
        seen, total, times = set(), 0, []
        with tarfile.open(path, "r:gz") as archive:
            for member in archive:
                relative = PurePosixPath(member.name)
                if (not relative.parts or relative.is_absolute() or ".." in relative.parts
                        or "\\" in member.name or ":" in member.name
                        or relative.as_posix() != member.name.rstrip("/") or relative.as_posix() in seen
                        or any(ord(character) < 32 for character in member.name)
                        or member.type not in {tarfile.DIRTYPE, tarfile.REGTYPE} or member.mode & ~0o777
                        or member.size < 0 or (member.isdir() and member.size != 0)):
                    raise RuntimeError("Unsafe compiler archive path/type/mode")
                seen.add(relative.as_posix())
                total += member.size
                if len(seen) > MEMBER_LIMIT or total > ARCHIVE_LIMIT:
                    raise RuntimeError("Compiler archive exceeds its supported inventory")
                destination = staged.joinpath(*relative.parts)
                destination.parent.mkdir(parents=True, exist_ok=True)
                if member.isdir():
                    destination.mkdir(exist_ok=True)
                else:
                    with archive.extractfile(member) as source, destination.open("xb") as stream:
                        shutil.copyfileobj(source, stream)
                destination.chmod(member.mode)
                instant = int(Decimal(member.pax_headers.get("mtime", str(member.mtime))) * 1_000_000_000)
                times.append((destination, instant))
        if len(seen) != description["members"] or total != description["bytes"]:
            raise RuntimeError("Compiler archive inventory differs from its producer")
        for destination, instant in reversed(times):
            os.utime(destination, ns=(instant, instant))
        identity = json.loads((staged / ".ci-cache/source-identity.json").read_text(encoding="utf-8"))
        if identity != {"version": 2, "pin": document["inputs"]["pin"],
                        "compiler_context": cache.compiler_context(document["inputs"])}:
            raise RuntimeError("Archived compiler-input identity differs from its original key")
        count = cache.verify(staged, document["inputs"]["pin"])
        staged.rename(target)
    write(output / "import.json", {"key": key, "tracked_source_files": count, "archive": description,
                                  "imported_programs_executed": False, "test_results_reused": False})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=["export", "select", "import"])
    parser.add_argument("--root", type=Path, default=Path.cwd())
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--journey", type=Path)
    parser.add_argument("--data", type=Path)
    parser.add_argument("--include-archive", choices=["true", "false"], default="true")
    args = parser.parse_args()
    root, output = args.root.resolve(), args.output.resolve()
    if args.command == "export":
        if args.journey is None or os.environ.get("GITHUB_EVENT_NAME") != "merge_group":
            parser.error("export requires the original merge-group journey")
        export(root, output, args.journey.resolve(), args.include_archive == "true")
    else:
        api = GitHub(root)
        head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip()
        if (os.environ.get("GITHUB_EVENT_NAME") != "push" or os.environ.get("GITHUB_REF") != "refs/heads/main"
                or os.environ.get("GITHUB_REPOSITORY") != REPOSITORY or os.environ.get("GITHUB_SHA") != head):
            parser.error("compiler publication requires the exact protected-main push checkout")
        if args.command == "select":
            result = select(root, output, api, head)
            with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as stream:
                for name in ["available", "archive", "key", "artifact_id", "run_id"]:
                    value = result.get(name, "")
                    stream.write(f"{name}={str(value).lower() if isinstance(value, bool) else value}\n")
        else:
            if args.data is None:
                parser.error("import requires the downloaded original compiler data")
            import_archive(root, output, args.data.resolve(), api, head)


if __name__ == "__main__":
    main()
