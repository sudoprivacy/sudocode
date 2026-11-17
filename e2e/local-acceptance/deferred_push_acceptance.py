"""Exercise the deferred push with real Git, SSH signing and native children.

A follow-up push to a ready PR publishes its signed plan, returns, and verifies
the exact head in the branch's own worktree in the background. A newer head
supersedes the older check, the same head never runs twice, and every other
push keeps the original blocking path. The compiler-cache probe is exercised
with a stand-in server that never answers. GitHub metadata is faked in process
and the background finalization request is made without credentials, so no
call reaches GitHub. This grants no product journey coverage.
"""
import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time


SOURCE = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def wait_for(predicate, what, timeout=120):
    deadline = time.monotonic() + timeout
    while not predicate():
        if time.monotonic() > deadline:
            raise AssertionError(f"timed out waiting for {what}")
        time.sleep(0.2)


def run(output):
    output.mkdir(parents=True, exist_ok=False)
    spawned = []
    try:
        exercise(output, spawned)
    finally:
        # Release and stop every background check, so a failed assertion
        # reports itself instead of leaving children holding the fixture.
        control = output / "control"
        if control.is_dir():
            for name in ["release-first", "release-second"]:
                (control / name).write_text("stop")
        import psutil

        for pid in spawned:
            try:
                process = psutil.Process(pid)
            except psutil.NoSuchProcess:
                continue
            for child in process.children(recursive=True)[::-1] + [process]:
                try:
                    child.kill()
                except psutil.NoSuchProcess:
                    pass


def exercise(output, spawned):
    parent = load("deferred_environment", SOURCE / "e2e/local-acceptance/run.py")
    os.environ.clear()
    os.environ.update(parent.FIXTURE_ENV)
    # The background finalization runs gh without credentials, so it fails
    # locally instead of reaching GitHub for this fixture repository.
    for name in ["GH_TOKEN", "GITHUB_TOKEN", "GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"]:
        os.environ.pop(name, None)
    os.environ["GH_CONFIG_DIR"] = str(output / "gh-config")
    control = output / "control"
    control.mkdir()
    os.environ["SCODE_DEFERRED_FIXTURE_CONTROL"] = str(control)
    work, origin = output / "author", output / "origin.git"
    work.mkdir()

    def git(*args, cwd=work):
        result = subprocess.run(["git", *args], cwd=cwd, capture_output=True)
        assert result.returncode == 0, result.stderr.decode(errors="replace")
        return result.stdout.decode().strip()

    git("init", "--bare", "--initial-branch=main", str(origin))
    git("init", "--initial-branch=main")
    for name, setting in [("user.name", "Deferred fixture"), ("user.email", "fixture@example.invalid"),
                          ("core.autocrlf", "false")]:
        git("config", name, setting)
    repository = "fixture/deferred-push"
    remote_url = "https://github.com/" + repository + ".git"
    git("remote", "add", "origin", remote_url)
    git("config", "url." + origin.as_uri() + ".insteadOf", remote_url)
    git("remote", "add", "elsewhere", origin.as_uri())
    key = output / "fixture-key"
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key)], check=True)
    helper = load("deferred_owner", SOURCE / ".github/scripts/local_acceptance.py")
    platform = helper.host()
    for relative in [".github/scripts/local_acceptance.py", ".github/local-acceptance/gate.py",
                     ".github/local-acceptance/publish.py", ".github/local-acceptance/cross_platform.py",
                     ".github/workflows/local-acceptance.yml", ".github/workflows/local-acceptance-publish.yml"]:
        destination = work / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(SOURCE / relative, destination)
    policy = {"version": 1, "repository": repository, "checks": {"native-workspace": {"include": ["input.txt"]}}}
    (work / "spec").mkdir()
    (work / "spec/local_acceptance.json").write_text(json.dumps(policy))
    (work / ".gitignore").write_text("__pycache__/\n")
    (work / "spec/local_acceptance_signers.json").write_text(json.dumps({"version": 1, "signers": {
        "fixture": {"platform": platform["system"], "public_key": key.with_suffix(".pub").read_text().strip()}}}))
    (work / ".github/local-acceptance/Dockerfile").write_text("FROM scratch\n")
    (work / ".github/local-acceptance/checks.py").write_text(
        'import sys\ndef run(name, root, output, base, execute):\n'
        ' output.mkdir(parents=True, exist_ok=True)\n'
        ' return [execute([sys.executable, str(root/"native-child.py")], root, output/"000.log")]\n')
    # The child records which source it ran on and waits for its release, so
    # the fixture can supersede a check that is still running.
    (work / "native-child.py").write_text(
        'import os,time\nfrom pathlib import Path\n'
        'control=Path(os.environ["SCODE_DEFERRED_FIXTURE_CONTROL"])\n'
        'source=Path("input.txt").read_text()\n'
        '(control/("started-"+source)).write_text(str(os.getpid()))\n'
        'deadline=time.monotonic()+90\n'
        'while not (control/("release-"+source)).exists():\n'
        ' if time.monotonic()>deadline: raise SystemExit(23)\n'
        ' time.sleep(0.05)\n'
        'print("checked "+source)\n')
    (work / "input.txt").write_text("before")
    git("add", ".")
    git("-c", "commit.gpgsign=false", "commit", "-m", "Protected fixture")
    git("push", "origin", "main")
    trusted = git("rev-parse", "HEAD")
    git("checkout", "-b", "source")
    git("config", "localAcceptance.signer", "fixture")
    git("config", "localAcceptance.key", str(key))
    helper.ROOT = work

    def commit(text):
        (work / "input.txt").write_text(text)
        git("add", "input.txt")
        git("-c", "commit.gpgsign=false", "commit", "-m", text)
        return git("rev-parse", "HEAD")

    first = commit("first")
    git("fetch", "origin", "+refs/heads/main:refs/remotes/origin/main")
    preview = helper.acceptance_plan(policy, ["native-workspace"], platform)
    assert preview["ci"], preview
    ready = {"number": 7, "draft": False, "head": {"repo": {"full_name": repository}}}
    requests = []

    def fake_api(answer):
        def api(path, payload=None):
            requests.append(path)
            return answer
        return api

    # Only exactly one open, ready PR from this repository defers the push.
    cases = [([], None), ([{**ready, "draft": True}], None), ([ready, {**ready, "number": 8}], None),
             ([{**ready, "head": {"repo": {"full_name": "someone/fork"}}}], None), ([ready], 7)]
    for answer, expected in cases:
        helper.github_api = fake_api(answer)
        found = helper.deferred_pull("origin", first, preview)
        assert (found and found["number"]) == expected, (answer, found)
    assert all("pulls?state=open&base=main&head=fixture%3Asource" in path for path in requests), requests
    helper.github_api = fake_api([ready])
    os.environ["SCODE_ACCEPTANCE_PUSH"] = "blocking"
    assert helper.deferred_pull("origin", first, preview) is None
    del os.environ["SCODE_ACCEPTANCE_PUSH"]
    assert helper.deferred_pull("elsewhere", first, preview) is None, "a non-GitHub remote keeps blocking"
    assert helper.deferred_pull("origin", first, {**preview, "ci": []}) is None, "no complementary host keeps blocking"

    def remote_note(note_ref, head):
        fetched = subprocess.run(["git", "fetch", "--no-tags", "origin", f"+{note_ref}:{note_ref}"], cwd=work,
                                 capture_output=True)
        if fetched.returncode:
            return None
        shown = subprocess.run(["git", "notes", "--ref", note_ref, "show", head], cwd=work, capture_output=True)
        return json.loads(shown.stdout) if shown.returncode == 0 else None

    def record():
        current = json.loads((helper.deferred_state("source") / "deferred.json").read_text(encoding="utf-8"))
        spawned.append(current["pid"])
        return current

    # The first head: plan published at once, check running in its own worktree.
    started = time.monotonic()
    helper.defer_check(first, "origin", ready, trusted)
    assert time.monotonic() - started < 60, "the push must not wait for the check"
    assert remote_note(helper.PLAN_NOTE_REF, first)["attestations"], "the signed plan precedes the push"
    first_record = record()
    worktree = helper.deferred_state("source") / "source"
    assert git("rev-parse", "HEAD", cwd=worktree) == first
    wait_for(lambda: (control / "started-first").exists(), "the first check to start")
    assert git("status", "--porcelain") == "", "the author's checkout is untouched"

    # A newer head supersedes it: the old check stops before publishing.
    second = commit("second")
    helper.defer_check(second, "origin", ready, trusted)
    import psutil

    wait_for(lambda: not psutil.pid_exists(first_record["pid"]) or psutil.Process(first_record["pid"]).status()
             == psutil.STATUS_ZOMBIE, "the superseded check to stop")
    assert json.loads((Path(first_record["output"]) / "superseded.json").read_text())["superseded_by"] == second
    second_record = record()
    assert second_record["head"] == second and second_record["pid"] != first_record["pid"]
    wait_for(lambda: (control / "started-second").exists(), "the second check to start")

    # The same head again keeps the running check instead of starting another.
    helper.defer_check(second, "origin", ready, trusted)
    assert record()["pid"] == second_record["pid"]

    (control / "release-second").write_text("go")
    (control / "release-first").write_text("go")
    log = Path(second_record["log"])
    wait_for(lambda: not psutil.pid_exists(second_record["pid"]), "the second check to finish", timeout=300)
    text = log.read_text(encoding="utf-8", errors="replace")
    receipt = remote_note(helper.NOTE_REF, second)
    assert receipt and receipt["attestations"], text
    assert f"requesting trusted finalization for PR #7 at {second[:12]}" in text, text
    assert remote_note(helper.NOTE_REF, first) is None, "a superseded head publishes no receipt"

    # Compiler cache: a server that never answers the probe means original Cargo.
    cache = load("deferred_compiler_cache", SOURCE / ".github/local-acceptance/compiler_cache.py")
    cache.PROBE_SECONDS = 2
    with tempfile.TemporaryDirectory(prefix="scode-cache-probe-") as scratch:
        session = cache.Session({"executable": sys.executable}, Path(scratch), None, cache=Path(scratch) / "objects")
        session.environment = {}
        (Path(scratch) / "probe").mkdir()
        # The stand-in tool runs `rustc` from the probe directory as a script.
        (Path(scratch) / "probe/rustc").write_text("import time\ntime.sleep(30)\n")
        began = time.monotonic()
        assert session.answers_probe() is False
        assert time.monotonic() - began < 15, "the probe is bounded"
        (Path(scratch) / "probe/rustc").write_text("")
        assert session.answers_probe() is True
        session.owned_servers = lambda marker=None: []
        session.fall_back("fixture server did not answer")
        assert session.disabled
    print("DEFERRED_PUSH_ACCEPTANCE=" + json.dumps({"plan_before_push": True, "superseded": True,
                                                     "same_head_kept": True, "receipt_published": True,
                                                     "blocking_kept": True, "cache_probe": True}))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", type=Path, required=True)
    run(parser.parse_args().output.resolve())
