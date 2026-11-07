"""Exercise parallel publication with real Git/SSH/HTTP and native children.

The HTTP fixture models GitHub metadata. Its native children all run on the
author OS; this grants no Linux/macOS/Windows product journey coverage.
"""
import argparse
import base64
from datetime import datetime, timezone
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import urllib.parse
import urllib.request


SOURCE = Path(__file__).resolve().parents[2]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def run_case(output, order):
    output.mkdir(parents=True, exist_ok=False)
    parent = load("parallel_environment", SOURCE / "e2e/local-acceptance/run.py")
    os.environ.clear()
    os.environ.update(parent.FIXTURE_ENV)
    work, origin, trusted = output / "author", output / "origin.git", output / "trusted"
    work.mkdir()
    commands, requests, processes, failures = [], [], [], []

    def git(*args, cwd=work):
        result = subprocess.run(["git", *args], cwd=cwd, capture_output=True)
        commands.append({"command": ["git", *args], "exit_code": result.returncode,
                         "stdout": result.stdout.decode(errors="replace"), "stderr": result.stderr.decode(errors="replace")})
        assert result.returncode == 0, result.stderr.decode(errors="replace")
        return result.stdout.decode().strip()

    git("init", "--bare", "--initial-branch=main", str(origin))
    git("init", "--initial-branch=main")
    git("config", "user.name", "Parallel native fixture")
    git("config", "user.email", "fixture@example.invalid")
    git("config", "core.autocrlf", "false")
    repository = "fixture/parallel-native"
    remote_url = "https://github.com/" + repository + ".git"
    git("remote", "add", "origin", remote_url)
    git("config", "url." + origin.as_uri() + ".insteadOf", remote_url)
    key = output / "fixture-key"
    subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key)], check=True)
    platform = load("parallel_owner", SOURCE / ".github/scripts/local_acceptance.py").host()
    for relative in [".github/scripts/local_acceptance.py", ".github/local-acceptance/gate.py",
                     ".github/local-acceptance/publish.py", ".github/workflows/local-acceptance.yml",
                     ".github/workflows/local-acceptance-publish.yml"]:
        destination = work / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(SOURCE / relative, destination)
    policy = {"version": 1, "repository": repository,
              "checks": {"native-workspace": {"include": ["input.txt"]}}}
    (work / "spec").mkdir()
    (work / "spec/local_acceptance.json").write_text(json.dumps(policy))
    (work / ".gitignore").write_text("__pycache__/\n")
    (work / "spec/local_acceptance_signers.json").write_text(json.dumps({"version": 1, "signers": {
        "fixture": {"platform": platform["system"], "public_key": key.with_suffix(".pub").read_text().strip()}}}))
    (work / ".github/local-acceptance/Dockerfile").write_text("FROM scratch\n")
    (work / ".github/local-acceptance/checks.py").write_text(
        'import sys\ndef run(name, root, output, base, execute):\n'
        ' output.mkdir(parents=True, exist_ok=True)\n'
        ' return [execute([sys.executable, str(root/"native-child.py"), str(output)], root, output/"000.log")]\n')
    control = output / "control"
    control.mkdir()
    (work / "native-child.py").write_text(
        'import os,sys,time\nfrom pathlib import Path\n'
        'control=Path(os.environ["SCODE_PARALLEL_FIXTURE_CONTROL"])\n'
        'assert Path("input.txt").read_text()=="pass"\n'
        '(control/"author-started").write_text(str(time.time()))\n'
        'deadline=time.monotonic()+30\n'
        'while not (control/"author-release").exists():\n'
        ' if time.monotonic()>deadline: raise SystemExit(23)\n'
        ' time.sleep(0.02)\n'
        '(control/"author-finished").write_text(str(time.time()))\n'
        'print("actual author child validated current source")\n')
    (work / "input.txt").write_text("before")
    git("add", ".")
    git("-c", "commit.gpgsign=false", "commit", "-m", "Protected fixture")
    base = git("rev-parse", "HEAD")
    git("push", "origin", "main")
    git("checkout", "-b", "source")
    (work / "input.txt").write_text("pass")
    git("add", ".")
    git("-c", "commit.gpgsign=false", "commit", "-m", "Frozen fixture source")
    head = git("rev-parse", "HEAD")
    git("config", "localAcceptance.signer", "fixture")
    git("config", "localAcceptance.key", str(key))
    git("clone", str(origin), str(trusted), cwd=output)
    git("config", "url." + origin.as_uri() + ".insteadOf", remote_url, cwd=trusted)
    helper = load("parallel_fixture_helper", work / ".github/scripts/local_acceptance.py")
    helper.ROOT = work
    publisher = load("parallel_fixture_publisher", trusted / ".github/local-acceptance/publish.py")
    publisher.gate.acceptance.ROOT = trusted
    plan = helper.acceptance_plan(policy, ["native-workspace"], platform)
    pull = {"number": 1, "node_id": "fixture-node", "state": "open", "draft": True,
            "head": {"sha": head, "repo": {"full_name": repository}}, "base": {"ref": "main"}}
    native_jobs = [{"id": index + 2, "name": job["name"], "conclusion": "success", "status": "completed"}
                   for index, job in enumerate(plan["ci"])]
    source_run = {"id": 101, "name": "Local acceptance", "display_title": "Local acceptance",
                  "path": ".github/workflows/local-acceptance.yml", "status": "completed", "conclusion": "success",
                  "event": "pull_request", "head_sha": head, "run_attempt": 1}
    publisher_run = {"id": 902, "path": ".github/workflows/local-acceptance-publish.yml",
                     "event": "workflow_dispatch", "head_branch": "main", "run_attempt": 1,
                     "status": "completed", "conclusion": "success",
                     "display_title": "Local acceptance publish " + head}
    statuses, observed, started_workers = [], [], []
    server = None
    coordinator = None
    original_urlopen = urllib.request.urlopen

    def wait_for(path):
        deadline = time.monotonic() + 30
        while not path.exists():
            if time.monotonic() > deadline:
                raise RuntimeError("Actual native child did not reach " + str(path))
            time.sleep(0.02)

    def publish(event, identifier):
        event_file = output / f"event-{identifier}.json"
        event_file.write_text(json.dumps(event))
        os.environ.update(GITHUB_REPOSITORY=repository, GITHUB_RUN_ID=str(identifier),
            GITHUB_EVENT_PATH=str(event_file), GITHUB_OUTPUT=str(output / "github-output"), GH_TOKEN="fixture-not-a-credential")
        publisher.main()

    def coordinate():
        try:
            wait_for(control / "author-started")
            if order == "author-first":
                (control / "author-release").write_text("finish first")
                return
            for process in processes:
                assert process.wait(timeout=20) == 0
            observed.append("native-ci-finished-before-author")
            publish({"workflow_run": source_run}, 901)
            assert statuses[-1]["state"] == "pending"
            assert not (control / "author-finished").exists()
            (control / "author-release").write_text("native jobs retained")
        except BaseException as error:
            failures.append(repr(error))
            (control / "author-release").write_text("release failed fixture")

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            self.handle_request()
        def do_POST(self):
            self.handle_request()
        def log_message(self, *_args):
            pass
        def handle_request(self):
            nonlocal coordinator
            try:
                body = json.loads(self.rfile.read(int(self.headers.get("Content-Length", "0"))) or b"null")
                parsed = urllib.parse.urlparse(self.path)
                path = parsed.path
                requests.append({"method": self.command, "path": self.path, "body": body})
                if path == "/graphql":
                    pull["draft"] = False
                    git("--git-dir", str(origin), "update-ref", "refs/pull/1/head", head)
                    for index, _job in enumerate(native_jobs):
                        worker = output / f"worker-{index}.py"
                        worker.write_text('from pathlib import Path\nimport sys,time\n'
                            'assert Path(sys.argv[1]).read_text()=="pass"\n'
                            'deadline=time.monotonic()+30\n'
                            'while not Path(sys.argv[2]).exists():\n'
                            ' if time.monotonic()>deadline: raise SystemExit(24)\n'
                            ' time.sleep(0.02)\n')
                        release = control / ("author-started" if order == "ci-first" else "ci-release")
                        process = subprocess.Popen([sys.executable, str(worker), str(work / "input.txt"), str(release)])
                        processes.append(process)
                        started_workers.append(process.pid)
                    coordinator = threading.Thread(target=coordinate, daemon=True)
                    coordinator.start()
                    result = {"data": {"markPullRequestReadyForReview": {"pullRequest": {"isDraft": False}}}}
                elif path.endswith("/pulls"):
                    result = pull if self.command == "POST" else [pull]
                elif path.endswith("/pulls/1"):
                    result = pull
                elif path.endswith("/actions/runs/101"):
                    result = source_run
                elif path.endswith("/actions/runs/902"):
                    result = publisher_run
                elif path.endswith("/actions/workflows/local-acceptance.yml/runs"):
                    result = {"workflow_runs": [source_run]}
                elif "/actions/runs/101/attempts/" in path and path.endswith("/jobs"):
                    assert all(process.poll() == 0 for process in processes)
                    result = {"jobs": [{"id": 1, "name": "Verify local acceptance", "conclusion": "success"}, *native_jobs]}
                elif path.endswith("/statuses/" + head):
                    statuses.append(body)
                    result = {"state": body["state"]}
                elif path.endswith("/actions/workflows/local-acceptance-publish.yml/dispatches"):
                    assert body == {"ref": "main", "inputs": {"pr": "1", "head": head}}
                    if order == "author-first":
                        assert (control / "author-finished").exists()
                        (control / "ci-release").write_text("finish retained native jobs")
                        for process in processes:
                            assert process.wait(timeout=20) == 0
                        observed.append("author-finished-before-native-ci")
                    # Dispatch itself completes without waiting on another
                    # handler; publication below uses independent HTTP calls.
                    result = None
                else:
                    raise AssertionError("Unexpected metadata request " + self.path)
                self.send_response(204 if result is None else 200)
                self.end_headers()
                if result is not None:
                    self.wfile.write(json.dumps(result).encode())
            except BaseException as error:
                failures.append(repr(error))
                self.send_response(500)
                self.end_headers()

    try:
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        address = f"http://127.0.0.1:{server.server_port}"
        def http_api(path, body=None):
            request = urllib.request.Request(address + "/" + path,
                data=json.dumps(body).encode() if body is not None else None,
                headers={"Content-Type": "application/json"})
            with original_urlopen(request, timeout=30) as response:
                raw = response.read()
            return json.loads(raw) if raw else None
        def redirected(request, *args, **kwargs):
            if isinstance(request, urllib.request.Request) and request.full_url.startswith("https://api.github.com/"):
                request = urllib.request.Request(address + request.full_url.removeprefix("https://api.github.com"),
                    data=request.data, headers=dict(request.header_items()), method=request.get_method())
            return original_urlopen(request, *args, **kwargs)
        urllib.request.urlopen = redirected
        os.environ["SCODE_PARALLEL_FIXTURE_CONTROL"] = str(control)
        args = argparse.Namespace(remote="origin", output=str(output / "execution"), title="Parallel fixture", body_file=None)
        helper.start_pr(args, api=http_api)
        coordinator.join(timeout=30)
        assert not coordinator.is_alive() and not failures, failures
        assert len(started_workers) == 2 and all(process.poll() == 0 for process in processes)
        publish({"inputs": {"pr": "1", "head": head}}, 902)
        assert statuses[-1]["state"] == "success"
        assert len(started_workers) == 2, "Finalization reran a completed native job"
        if order == "ci-first":
            assert [row["state"] for row in statuses] == ["pending", "success"]
        git("fetch", "origin", "+refs/notes/local-acceptance:refs/notes/local-acceptance", cwd=trusted)
        full = publisher.gate.acceptance.verify(head, base, base)
        receipt = json.loads((output / "execution/author/receipt.json").read_text())
        assert full == receipt
        outer = json.loads((output / "execution/parallel-attempt.json").read_text())
        assert receipt["timing"]["first_recorded_started_at"] <= outer["started_at"]
        positive_states = [row["state"] for row in statuses]
        # A retry reuses both the actual completed author signature and the
        # same native job processes; the original command log stays untouched.
        author_log = output / "execution/author/native-workspace/000.log"
        before = helper.sha(author_log.read_bytes()), author_log.stat().st_mtime_ns
        retry = argparse.Namespace(remote="origin", output=str(output / "retry"), title="Parallel fixture", body_file=None)
        helper.start_pr(retry, api=http_api)
        assert not (output / "retry/author").exists()
        assert before == (helper.sha(author_log.read_bytes()), author_log.stat().st_mtime_ns)
        assert len(started_workers) == 2

        refused = []
        envelope = next(iter(helper.read_note(head, helper.PLAN_NOTE_REF)["attestations"].values()))
        pending = json.loads(base64.b64decode(envelope["payload"]))
        for label, action in [
            ("plan-is-not-full", lambda: helper.verify_envelope(envelope, head, base, base)),
            ("plan-is-not-checkpoint", lambda: helper.verify_envelope(envelope, head, base, base, completed_only=True)),
        ]:
            try:
                action()
            except RuntimeError:
                refused.append(label)
            else:
                raise AssertionError("Execution plan satisfied " + label)
        variants = {
            "changed-signed-head": {**pending, "head": base},
            "reduced-native-plan": {**pending, "plan": {**pending["plan"], "ci": []}},
            "claimed-completed-checks": {**pending, "checks": {"native-workspace": []}},
            "unprotected-policy": {**pending, "policy": head},
        }
        for label, contents in variants.items():
            signed = helper.sign_receipt(contents, output / "negative-signatures" / label,
                                         "fixture", key, namespace=helper.NAMESPACE + "-plan")
            try:
                helper.verify_execution_plan(signed, head, base, base)
            except RuntimeError:
                refused.append(label)
            else:
                raise AssertionError("Signed plan accepted " + label)
        publisher.gate.verify_publisher(repository, "902", statuses[-1], head=head, require_bound=True)
        for label, field, value in [
            ("publisher-other-head", "display_title", "Local acceptance publish " + "0" * 40),
            ("publisher-unprotected-branch", "head_branch", "source"),
            ("publisher-other-workflow", "path", ".github/workflows/other.yml"),
        ]:
            original = publisher_run[field]
            publisher_run[field] = value
            try:
                publisher.gate.verify_publisher(repository, "902", statuses[-1], head=head, require_bound=True)
            except RuntimeError:
                refused.append(label)
            else:
                raise AssertionError("Publisher accepted " + label)
            finally:
                publisher_run[field] = original

        # This separate later failed-attempt control runs a real failing
        # native process. It must supersede the earlier successful fixture run.
        failed = subprocess.run([sys.executable, "-c", "raise SystemExit(19)"], capture_output=True)
        assert failed.returncode == 19
        source_run.update(run_attempt=2, conclusion="failure")
        native_jobs[0]["conclusion"] = "failure" if failed.returncode else "success"
        for label, event, identifier in [
            ("later-native-failure", {"workflow_run": source_run}, 903),
            ("finalize-cannot-hide-native-failure", {"inputs": {"pr": "1", "head": head}}, 904),
        ]:
            try:
                publish(event, identifier)
            except RuntimeError:
                assert statuses[-1]["state"] == "failure"
                refused.append(label)
            else:
                raise AssertionError("A failed native process satisfied final acceptance")
        count = len(statuses)
        publish({"workflow_run": {**source_run, "run_attempt": 1}}, 905)
        assert len(statuses) == count and statuses[-1]["state"] == "failure"
        assert len(started_workers) == 2
        result = {"status": "PASS", "order": order, "head": head, "base": base, "observed": observed,
            "states": positive_states, "all_statuses": statuses, "native_pids": started_workers,
            "refused": refused, "actual_negative_child_exit": failed.returncode,
            "retry_preserves_author_log_and_native_processes": True,
            "native_jobs_executed_once": True, "complete_actual_author_signature_verified": True,
            "outer_setup_included_in_signed_timing": True, "commands": commands, "requests": requests,
            "scope": "Real local Git remote/hooks, SSH, HTTP metadata, author command and native children. Metadata models GitHub; every actual child uses this one OS. No hosted or cross-OS coverage inferred."}
        (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        return result
    finally:
        urllib.request.urlopen = original_urlopen
        if server:
            server.shutdown()
            server.server_close()
        for process in processes:
            if process.poll() is None:
                process.terminate()
                process.wait(timeout=10)
        (output / "metadata-journal.json").write_text(json.dumps({"commands": commands, "requests": requests,
            "states": statuses, "failures": failures, "observed": observed}, indent=2) + "\n")


def run(output):
    output.mkdir(parents=True, exist_ok=False)
    helper = load("parallel_control_lease", SOURCE / ".github/scripts/local_acceptance.py")
    helper.ROOT = SOURCE
    attempt = {"active_profile": "private-parallel-native-protocol-controls"}
    summary = {"started_at": datetime.now(timezone.utc).isoformat(),
               "scope": "Actual local Git/SSH/HTTP/native protocol controls; no hosted platform/product coverage."}
    started = time.monotonic()
    try:
        revision = helper.git("rev-parse", "--verify", "HEAD", check=False)
        source_identity = (revision.stdout.decode().strip() if revision.returncode == 0
                           else "private-tool-fixture-without-source-commit")
        with helper.host_acceptance_lease(attempt, source_identity):
            results = [run_case(output / order, order) for order in ["ci-first", "author-first"]]
        summary.update(status="PASS", cases=[{"order": row["order"], "states": row["states"]} for row in results])
    except BaseException as error:
        summary.update(status="FAIL", error=repr(error))
        raise
    finally:
        summary.update(finished_at=datetime.now(timezone.utc).isoformat(), seconds=time.monotonic() - started, lease=attempt)
        (output / "result.json").write_text(json.dumps(summary, indent=2) + "\n")
        print(json.dumps(summary), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    run(args.output.resolve())
