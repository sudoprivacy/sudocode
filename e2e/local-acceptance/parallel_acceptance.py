"""Exercise parallel publication with real Git/SSH/HTTP and native children.

The HTTP fixture models GitHub metadata. Its native children all run on the
author OS; this grants no Linux/macOS/Windows product journey coverage.
"""
import argparse
import base64
import copy
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


def private_protocol_environment():
    fixture = load("protocol_fixture_environment", SOURCE / "e2e/local-acceptance/fixture_environment.py")
    # These cases launch only the synthetic Python profiles, Git and SSH.
    # Keep their private home and lease isolation without copying build tools.
    _, environment = fixture.create_fixture_environment(SOURCE, seed_prepared_tools=False)
    return environment


def run_case(output, order):
    output.mkdir(parents=True, exist_ok=False)
    environment = private_protocol_environment()
    os.environ.clear()
    os.environ.update(environment)
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
    native_workflow = ".github/workflows/local-acceptance.yml"
    toolchain_adapter = ".github/actions/rust-toolchain/cache-command.mjs"
    layout_runner = ".github/local-acceptance/cache_layout.py"
    relocated_workflow = (SOURCE / native_workflow).read_text(encoding="utf-8").encode()
    assert relocated_workflow.count(b"worktree") == 5
    cache_workflow = relocated_workflow.replace(b"worktree", b"candidate")
    cache_output = b"          cmd-format: ${{ steps.rust_toolchain.outputs.dependency-cache-command }}\n"
    assert cache_workflow.count(cache_output) == cache_workflow.count(b"        id: rust_toolchain\n") == 1
    original_workflow = cache_workflow.replace(cache_output, b"").replace(b"        id: rust_toolchain\n", b"")
    for relative in [".github/scripts/local_acceptance.py", ".github/local-acceptance/gate.py",
                     ".github/local-acceptance/publish.py", ".github/local-acceptance/cross_platform.py",
                     native_workflow, ".github/actions/rust-toolchain/action.yml", toolchain_adapter, layout_runner,
                     ".github/workflows/local-acceptance-workspace.yml",
                     ".github/workflows/local-acceptance-publish.yml"]:
        destination = work / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(SOURCE / relative, destination)
        if relative == native_workflow:
            destination.write_bytes(original_workflow if order == "ci-first" else cache_workflow)
        if relative.startswith(".github/actions/rust-toolchain/") or relative == layout_runner:
            destination.write_bytes(destination.read_text(encoding="utf-8").encode())
        if relative == ".github/scripts/local_acceptance.py":
            # Protected main is newer than the installed client. Its marker
            # proves that refresh executes the new code, not only new bytes.
            contents = destination.read_text(encoding="utf-8")
            anchor = "    args.output = str(output)\n"
            assert contents.count(anchor) == 1
            destination.write_text(contents.replace(anchor, anchor +
                '    (output / "protected-entry-code-ran.json").write_text(json.dumps({"started_at": context["started_at"], '
                '"refreshes": context["refreshes"], "observed_at": time.time()}))\n'), encoding="utf-8")
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
    git("config", "user.name", "Parallel native fixture", cwd=trusted)
    git("config", "user.email", "fixture@example.invalid", cwd=trusted)
    git("config", "core.autocrlf", "false", cwd=trusted)
    git("config", "url." + origin.as_uri() + ".insteadOf", remote_url, cwd=trusted)
    candidate_entry = SOURCE / ".github/scripts/local_acceptance.py"
    candidate_bytes = candidate_entry.read_bytes()
    refused_source = subprocess.run([sys.executable, str(candidate_entry), "start-pr", "--remote", "origin",
        "--output", str(output / "nonmanaged-refused")], cwd=work, capture_output=True)
    (output / "nonmanaged-refusal.log").write_bytes(refused_source.stdout + refused_source.stderr)
    assert refused_source.returncode != 0 and b"Candidate source was preserved" in refused_source.stderr
    assert candidate_entry.read_bytes() == candidate_bytes
    installed = subprocess.run([sys.executable, str(candidate_entry), "install"],
                               cwd=work, capture_output=True, check=True)
    (output / "original-install.log").write_bytes(installed.stdout + installed.stderr)
    managed = work / ".git/local-acceptance-hooks/local_acceptance.py"
    original_hook = (managed.parent / "pre-push").read_bytes()
    original_settings = {name: git("config", "--get", name)
                         for name in ["core.hooksPath", "localAcceptance.signer", "localAcceptance.key"]}
    original_source = git("status", "--porcelain"), git("rev-parse", "HEAD")
    assert managed.read_bytes() != (trusted / ".github/scripts/local_acceptance.py").read_bytes()
    helper = load("parallel_fixture_helper", managed)
    helper.ROOT = work
    publisher = load("parallel_fixture_publisher", trusted / ".github/local-acceptance/publish.py")
    publisher.gate.acceptance.ROOT = trusted
    plan = helper.acceptance_plan(policy, ["native-workspace"], platform)
    preview_plans = []
    for cap in [None, "1"]:
        environment = dict(os.environ)
        environment.pop("SCODE_ACCEPTANCE_TEST_THREADS", None)
        if cap is not None:
            environment["SCODE_ACCEPTANCE_TEST_THREADS"] = cap
        preview = subprocess.run([sys.executable, str(managed), "plan", "--head", head,
            "--base", base, "--policy-ref", base], cwd=work, env=environment, capture_output=True)
        (output / f"concurrency-preview-{cap or 'default'}.log").write_bytes(preview.stdout + preview.stderr)
        assert preview.returncode == 0, preview.stderr.decode("utf-8", errors="replace")
        lines = preview.stdout.decode("utf-8").splitlines()
        current = json.loads(next(line.split("=", 1)[1] for line in lines
                                  if line.startswith("LOCAL_ACCEPTANCE_CONCURRENCY=")))
        assert current["source"] == "current invocation environment"
        assert current["compiler_jobs_setting"] == "SCODE_ACCEPTANCE_BUILD_JOBS" and current["requested_test_worker_cap"] == cap
        assert (b"runs native test cases serially" in preview.stdout) == (cap == "1")
        preview_plans.append(json.loads(next(line.split("=", 1)[1] for line in lines
                                            if line.startswith("ACCEPTANCE_PLAN="))))
    assert preview_plans == [plan, plan], "Concurrency diagnostics must not change the signed plan"
    assert not (control / "author-started").exists(), "Plan preview must not start native execution"
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
        marker = json.loads((output / "execution/protected-entry-code-ran.json").read_text())
        initial_attempt = json.loads((output / "execution/parallel-attempt.json").read_text())
        assert marker["refreshes"] == initial_attempt["runner_refreshes"] == 1
        assert marker["started_at"] == initial_attempt["started_at"] <= marker["observed_at"]
        assert managed.read_bytes() == (trusted / ".github/scripts/local_acceptance.py").read_bytes()
        assert (managed.parent / "pre-push").read_bytes() == original_hook
        assert original_settings == {name: git("config", "--get", name) for name in original_settings}
        assert original_source == (git("status", "--porcelain"), git("rev-parse", "HEAD"))
        reinstalled = subprocess.run([sys.executable, str(managed), "install"], cwd=work, capture_output=True, check=True)
        (output / "idempotent-self-install.log").write_bytes(reinstalled.stdout + reinstalled.stderr)
        assert managed.read_bytes() == (trusted / ".github/scripts/local_acceptance.py").read_bytes()
        assert (managed.parent / "pre-push").read_bytes() == original_hook
        assert original_settings == {name: git("config", "--get", name) for name in original_settings}
        # Keep this already loaded old module for the first retry. The file is
        # current, but live code must still refresh before it can attest work.
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
        historical = subprocess.run([sys.executable, str(trusted / ".github/scripts/local_acceptance.py"),
            "verify", "--head", head, "--base", base, "--policy-ref", base], cwd=trusted,
            env={**os.environ, "SCODE_ACCEPTANCE_TEST_THREADS": "1"}, capture_output=True)
        (output / "historical-verify-with-current-cap.log").write_bytes(historical.stdout + historical.stderr)
        assert historical.returncode == 0, historical.stderr.decode("utf-8", errors="replace")
        assert b"LOCAL_ACCEPTANCE_CONCURRENCY=" not in historical.stdout
        assert b"runs native test cases serially" not in historical.stdout
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
        assert json.loads((output / "retry/parallel-attempt.json").read_text())["runner_refreshes"] == 1
        assert before == (helper.sha(author_log.read_bytes()), author_log.stat().st_mtime_ns)
        assert len(started_workers) == 2
        helper = load("parallel_fixture_refreshed_helper", managed)
        helper.ROOT = work
        current_retry = argparse.Namespace(remote="origin", output=str(output / "current-retry"),
                                           title="Parallel fixture", body_file=None)
        helper.start_pr(current_retry, api=http_api)
        assert json.loads((output / "current-retry/parallel-attempt.json").read_text())["runner_refreshes"] == 0
        assert not (output / "current-retry/author").exists()
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
            "changed-original-contract-hash": {**pending, "contract_sha256": "0" * 64},
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

        # A protected metadata update must not invalidate the still-authentic
        # launch plan or rerun its completed native jobs. Use actual newer Git
        # commits and the unchanged SSH notes, then reject changed execution.
        completed = next(iter(helper.read_note(head)["attestations"].values()))
        notes_before = helper.canonical(helper.read_note(head)), helper.canonical(helper.read_note(head, helper.PLAN_NOTE_REF))
        metadata_controls = []

        def protected_commit(relative, contents, message, parent):
            git("switch", "--detach", parent, cwd=trusted)
            destination = trusted / relative
            if isinstance(contents, bytes):
                destination.write_bytes(contents)
            else:
                destination.write_text(contents, encoding="utf-8")
            git("add", "--", relative, cwd=trusted)
            git("-c", "commit.gpgsign=false", "commit", "-m", message, cwd=trusted)
            changed = git("rev-parse", "HEAD", cwd=trusted)
            git("push", "origin", changed + ":refs/heads/main", cwd=trusted)
            git("fetch", "origin", "+refs/heads/main:refs/remotes/origin/main")
            return changed

        policy_path = "spec/local_acceptance.json"
        metadata = protected_commit(policy_path, json.dumps({**policy, "observation": "new diagnostics"}),
                                    "Advance protected metadata only", base)
        for label, relative, contents in [
            ("unrelated policy metadata", None, None),
            ("another enrolled signer", "spec/local_acceptance_signers.json", json.dumps({
                "version": 1, "signers": {
                    "fixture": {"platform": platform["system"], "public_key": key.with_suffix(".pub").read_text().strip()},
                    "additional": {"platform": platform["system"], "public_key": key.with_suffix(".pub").read_text().strip()},
                }})),
        ]:
            # The second independent update is a descendant of the first.
            if relative:
                metadata = protected_commit(relative, contents, "Keep the original enrolled signer", metadata)
            helper.verify_execution_plan(envelope, head, metadata, metadata)
            helper.verify_envelope(completed, head, metadata, metadata)
            publisher_run["head_sha"] = metadata
            publish({"workflow_run": source_run}, 906 + len(metadata_controls))
            assert statuses[-1]["state"] == "success"
            assert notes_before == (helper.canonical(helper.read_note(head)), helper.canonical(helper.read_note(head, helper.PLAN_NOTE_REF)))
            assert before == (helper.sha(author_log.read_bytes()), author_log.stat().st_mtime_ns)
            assert len(started_workers) == 2
            metadata_controls.append({"case": label, "policy": metadata, "accepted": True,
                                      "original_notes_and_processes_reused": True})

        # A maintained responsibility change keeps the author's complete local
        # work. Only the exact older, fuller CI split may survive unchanged.
        responsibility_controls = []
        mixed = {"checks": {"native-workspace": {}, "native-context": {},
                            "native-sdk": {"targets": ["linux-x64", "linux-arm64", "macos", "windows"]}}}
        names = list(mixed["checks"])
        periodic = copy.deepcopy(mixed)
        periodic["checks"]["native-workspace"]["complementary"] = "periodic"
        for system in ["windows", "linux", "macos"]:
            owner = {**platform, "system": system, "architecture": "x64"}
            full = helper.acceptance_plan(mixed, names, owner)
            current = helper.acceptance_plan(periodic, names, owner)
            expected = copy.deepcopy(full)
            for job in expected["ci"]:
                job["checks"] = [name for name in job["checks"] if name != "native-workspace"]
            assert current == expected
            assert current["local_checks"] == full["local_checks"] == names
            assert current["local_targets"] == full["local_targets"]
            assert helper.compatible_acceptance_plan(periodic, names, owner, full)
            assert not helper.compatible_acceptance_plan(mixed, names, owner, current)
            for label, mutate in [
                ("missing complementary host", lambda plan: plan["ci"].pop()),
                ("missing complementary context", lambda plan: plan["ci"][0]["checks"].remove("native-context")),
                ("changed complementary timeout", lambda plan: plan["ci"][0].update(timeout=1)),
                ("missing original author workspace", lambda plan: plan["local_checks"].remove("native-workspace")),
                ("changed original author", lambda plan: plan["author"].update(system="unknown")),
                ("claimed deployment", lambda plan: plan.update(deployment_verified=True)),
            ]:
                invalid = copy.deepcopy(full)
                mutate(invalid)
                assert not helper.compatible_acceptance_plan(periodic, names, owner, invalid), label
            responsibility_controls.append({"author": system, "full_author_and_other_profiles_retained": True,
                                            "only_exact_former_full_plan_accepted": True})
        for name, mode in [("native-workspace", value) for value in [None, False, [], "skip"]] + [
                ("native-context", "periodic"), ("native-sdk", "periodic")]:
            invalid = copy.deepcopy(mixed)
            invalid["checks"][name]["complementary"] = mode
            try:
                helper.acceptance_plan(invalid, names, platform)
            except RuntimeError:
                pass
            else:
                raise AssertionError("Unknown/foreign periodic responsibility was accepted")

        periodic_policy = copy.deepcopy(json.loads(git("show", metadata + ":" + policy_path)))
        periodic_policy["checks"]["native-workspace"]["complementary"] = "periodic"
        metadata = protected_commit(policy_path, json.dumps(periodic_policy),
                                    "Keep full author workspace; make complementary workspace periodic", metadata)
        assert helper.acceptance_plan(periodic_policy, ["native-workspace"], platform)["ci"] == []
        assert helper.verify_execution_plan(envelope, head, metadata, metadata) == pending
        original_completion = helper.verify_envelope(completed, head, metadata, metadata)
        assert original_completion["plan"] == pending["plan"]
        publisher_run["head_sha"] = metadata
        publish({"workflow_run": source_run}, 930)
        assert statuses[-1]["state"] == "success"
        helper.start_pr(argparse.Namespace(remote="origin", output=str(output / "periodic-retry"),
                        title="Parallel fixture", body_file=None), api=http_api)
        assert not (output / "periodic-retry/execution-plan").exists()
        assert not (output / "periodic-retry/author").exists()
        # Original signatures, note objects, logs and executed children stay
        # identical. A verified scheduling plan still proves no completed work.
        assert notes_before == (helper.canonical(helper.read_note(head)), helper.canonical(helper.read_note(head, helper.PLAN_NOTE_REF)))
        assert before == (helper.sha(author_log.read_bytes()), author_log.stat().st_mtime_ns)
        assert len(started_workers) == 2
        for label, mutate in [
            ("drop original author workspace", lambda plan: plan["local_checks"].clear()),
            ("incomplete former host split", lambda plan: plan["ci"].pop()),
            ("changed former host metadata", lambda plan: plan["ci"][0].update(node="18")),
        ]:
            invalid = copy.deepcopy(pending)
            mutate(invalid["plan"])
            signed = helper.sign_receipt(invalid, output / "periodic-negative" / label.replace(" ", "-"),
                                         "fixture", key, namespace=helper.NAMESPACE + "-plan")
            try:
                helper.verify_execution_plan(signed, head, metadata, metadata)
            except RuntimeError:
                refused.append(label)
            else:
                raise AssertionError("A signed partial legacy plan was accepted: " + label)
        metadata_controls.append({"case": "original full signed workspace plan and completion retained",
                                  "policy": metadata, "accepted": True,
                                  "original_notes_logs_and_processes_reused": True})
        # The completion verifier already honors protected exact recipe pairs.
        # Scheduling must share that decision without accepting a plan as a
        # test result or changing its original signatures/source responsibility.
        checks_path = ".github/local-acceptance/checks.py"
        original_checks = (trusted / checks_path).read_text()
        equivalent_checks = original_checks.replace('root/"native-child.py"', 'root.joinpath("native-child.py")')
        assert equivalent_checks != original_checks
        equivalent = protected_commit(checks_path, equivalent_checks, "Equivalent native child path construction", metadata)
        old_hash = helper.reachable_profile_commands_hashes(base, ["native-workspace"])["native-workspace"]
        new_hash = helper.reachable_profile_commands_hashes(equivalent, ["native-workspace"])["native-workspace"]
        assert old_hash != new_hash
        equivalent_policy = json.loads(git("show", equivalent + ":" + policy_path))
        for label, declaration in [
            ("unapproved recipe pair", {}),
            ("wrong predecessor", {"native-workspace": {new_hash: ["0" * 64]}}),
            ("wrong current recipe", {"native-workspace": {"0" * 64: [old_hash]}}),
            ("approval for another profile", {"native-services": {new_hash: [old_hash]}}),
            ("malformed predecessor list", {"native-workspace": {new_hash: old_hash}}),
            ("malformed approval table", []),
        ]:
            git("--git-dir", str(origin), "update-ref", "refs/heads/main", equivalent, cwd=output)
            candidate = protected_commit(policy_path, json.dumps({**equivalent_policy,
                "equivalent_profile_commands": declaration}), "Reject " + label, equivalent)
            try:
                helper.verify_execution_plan(envelope, head, candidate, candidate)
            except RuntimeError as error:
                metadata_controls.append({"case": label, "policy": candidate, "accepted": False, "reason": str(error)})
            else:
                raise AssertionError("Old plan accepted " + label)
        approved_policy = {**equivalent_policy,
                           "equivalent_profile_commands": {"native-workspace": {new_hash: [old_hash]}}}
        git("--git-dir", str(origin), "update-ref", "refs/heads/main", equivalent, cwd=output)
        approved = protected_commit(policy_path, json.dumps(approved_policy), "Approve the exact equivalent pair", equivalent)
        legacy_path = output / "old-plan-verifier.py"
        legacy_source = (trusted / ".github/scripts/local_acceptance.py").read_text()
        anchor = "approved = approved_equivalent_profiles(before, current, protected, changed, head)"
        assert legacy_source.count(anchor) == 1
        legacy_path.write_text(legacy_source.replace(anchor, "approved = {}"), encoding="utf-8")
        legacy = load("parallel_old_plan_verifier", legacy_path)
        legacy.ROOT = work
        assert legacy.compatible_profile_commands(json.loads(base64.b64decode(completed["payload"])),
                                                  approved, ["native-workspace"])
        try:
            legacy.verify_execution_plan(envelope, head, approved, approved)
        except RuntimeError:
            metadata_controls.append({"case": "old plan verifier rejects pair accepted by completion matching",
                                      "accepted": False, "negative_control": True})
        else:
            raise AssertionError("Old-plan negative control did not reproduce the inconsistency")
        helper.verify_execution_plan(envelope, head, approved, approved)
        helper.verify_envelope(completed, head, approved, approved)
        publisher_run["head_sha"] = approved
        publish({"workflow_run": source_run}, 940)
        assert statuses[-1]["state"] == "success"
        assert notes_before == (helper.canonical(helper.read_note(head)), helper.canonical(helper.read_note(head, helper.PLAN_NOTE_REF)))
        assert before == (helper.sha(author_log.read_bytes()), author_log.stat().st_mtime_ns)
        assert len(started_workers) == 2
        metadata_controls.append({"case": "protected exact pair shared by plan/completion/finalizer",
                                  "accepted": True, "old_and_new_recipes": [old_hash, new_hash],
                                  "original_notes_logs_and_processes_reused": True})
        metadata = approved
        if order == "ci-first":
            updated = protected_commit(native_workflow, cache_workflow, "Use the pinned cache key", metadata)
            assert helper.verify_execution_plan(envelope, head, updated, updated) == pending
            assert helper.verify_envelope(completed, head, updated, updated)["plan"] == pending["plan"]
            publisher_run["head_sha"] = updated
            publish({"workflow_run": source_run}, 941)
            assert statuses[-1]["state"] == "success"
            assert notes_before == (helper.canonical(helper.read_note(head)),
                                    helper.canonical(helper.read_note(head, helper.PLAN_NOTE_REF)))
            assert before == (helper.sha(author_log.read_bytes()), author_log.stat().st_mtime_ns)
            assert len(started_workers) == 2
            metadata_controls.append({"case": "reviewed cache-only workflow keeps original author and native checks",
                                      "accepted": True, "original_notes_logs_and_processes_reused": True})

            original_adapter = (trusted / toolchain_adapter).read_bytes()
            for label, relative, contents in [
                ("changed cache adapter", toolchain_adapter, original_adapter + b"\nprocess.exit(17);\n"),
                ("changed workflow deadline", native_workflow,
                 cache_workflow.replace(b"timeout-minutes: ${{ matrix.timeout }}", b"timeout-minutes: 20")),
            ]:
                git("--git-dir", str(origin), "update-ref", "refs/heads/main", updated, cwd=output)
                changed = protected_commit(relative, contents, "Refuse " + label, updated)
                try:
                    helper.verify_execution_plan(envelope, head, changed, changed)
                except RuntimeError as error:
                    assert "current protected native workflow definition" in str(error)
                    metadata_controls.append({"case": label, "accepted": False, "reason": str(error)})
                else:
                    raise AssertionError("An original scheduling plan accepted " + label)
        git("--git-dir", str(origin), "update-ref", "refs/heads/main", metadata, cwd=output)
        relocated = protected_commit(native_workflow, relocated_workflow, "Align the native cache input order", metadata)
        assert helper.verify_execution_plan(envelope, head, relocated, relocated) == pending
        assert helper.verify_envelope(completed, head, relocated, relocated)["plan"] == pending["plan"]
        publisher_run["head_sha"] = relocated
        publish({"workflow_run": source_run}, 942)
        assert statuses[-1]["state"] == "success"
        assert notes_before == (helper.canonical(helper.read_note(head)),
                                helper.canonical(helper.read_note(head, helper.PLAN_NOTE_REF)))
        assert before == (helper.sha(author_log.read_bytes()), author_log.stat().st_mtime_ns)
        assert len(started_workers) == 2
        metadata_controls.append({"case": "reviewed checkout relocation keeps original author and native checks",
                                  "accepted": True, "original_notes_logs_and_processes_reused": True})
        for label, relative, contents in [
            ("changed cache layout runner", layout_runner, (trusted / layout_runner).read_bytes() + b"\n# Modified layout\n"),
            ("only partly relocated checkout", native_workflow,
             relocated_workflow.replace(b"        working-directory: worktree\n", b"        working-directory: candidate\n")),
        ]:
            git("--git-dir", str(origin), "update-ref", "refs/heads/main", relocated, cwd=output)
            changed = protected_commit(relative, contents, "Refuse " + label, relocated)
            try:
                helper.verify_execution_plan(envelope, head, changed, changed)
            except RuntimeError as error:
                assert "current protected native workflow definition" in str(error)
                metadata_controls.append({"case": label, "accepted": False, "reason": str(error)})
            else:
                raise AssertionError("An original scheduling plan accepted " + label)
        helper_path = ".github/scripts/local_acceptance.py"
        executor = (trusted / helper_path).read_text()
        signature = next(line for line in executor.splitlines() if line.startswith("def run_command("))
        for label, relative, contents in [
            ("changed command assertion", checks_path,
             (trusted / checks_path).read_text().replace(' return [execute', ' assert False, "changed command assertion"\n return [execute')),
            ("changed process executor", helper_path,
             executor.replace(signature, signature + '\n    assert False, "changed process executor"', 1)),
            ("changed native workflow", ".github/workflows/local-acceptance.yml",
             (trusted / '.github/workflows/local-acceptance.yml').read_text() + '\n# Changed native workflow.\n'),
            ("changed native dispatcher", ".github/local-acceptance/cross_platform.py",
             (trusted / '.github/local-acceptance/cross_platform.py').read_text() + '\n# Changed native dispatch.\n'),
            ("changed acceptance image", ".github/local-acceptance/Dockerfile", 'FROM scratch\n# Changed image.\n'),
            ("newly required profile", policy_path, json.dumps({**policy, "checks": {
                **policy["checks"], "native-context": {"include": ["input.txt"]}}})),
            ("revoked original signer", "spec/local_acceptance_signers.json", json.dumps({"version": 1, "signers": {}})),
        ]:
            # Each refusal is its own forward-only protected branch. Keep the
            # origin transport on the accepted parent between these controls.
            git("--git-dir", str(origin), "update-ref", "refs/heads/main", metadata, cwd=output)
            changed = protected_commit(relative, contents, "Refuse " + label, metadata)
            try:
                helper.verify_execution_plan(envelope, head, changed, changed)
            except RuntimeError as error:
                metadata_controls.append({"case": label, "policy": changed, "accepted": False, "reason": str(error)})
            else:
                raise AssertionError("An old scheduling plan accepted " + label)
        git("--git-dir", str(origin), "update-ref", "refs/heads/main", base, cwd=output)
        git("switch", "--detach", base, cwd=trusted)
        git("fetch", "origin", "+refs/heads/main:refs/remotes/origin/main")
        publisher_run["head_sha"] = base

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
            "current_concurrency_preview_preserves_plan_and_execution": True,
            "historical_verification_excludes_current_environment": True,
            "outer_setup_included_in_signed_timing": True, "commands": commands, "requests": requests,
            "metadata_controls": metadata_controls,
            "workspace_responsibility_controls": responsibility_controls,
            "stale_installed_runner_refreshed_from_protected_main": True,
            "protected_new_code_actually_executed": True,
            "original_start_and_output_preserved": True,
            "hook_signer_key_and_frozen_source_preserved": True,
            "nonmanaged_candidate_refresh_refused_without_source_write": True,
            "idempotent_installed_self_install_passed": True,
            "already_loaded_stale_code_refreshed_even_with_current_file": True,
            "current_runner_retry_skipped_refresh_and_completed_work": True,
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


def plan_admission_controls(output):
    """Exercise the real check entry before and after compatible schema support."""
    output.mkdir(parents=True, exist_ok=False)
    environment = {**private_protocol_environment(), "PYTHONDONTWRITEBYTECODE": "1"}
    environment.pop("SCODE_SYNTHETIC_PLAN_PARSER_STATE", None)
    current = (SOURCE / ".github/scripts/local_acceptance.py").read_text(encoding="utf-8")
    anchor = '        complementary = policy["checks"][name].get("complementary")\n'
    start = current.index(anchor)
    end = current.index('        if not name.startswith("native-"):\n', start)
    # Reproduce the observed former parser: the new responsibility field is
    # ignored. All other verification/signing/process functions stay intact.
    legacy = current[:start] + "        complementary = None\n" + current[end:]
    admission = "    preflight_plan_compatibility(plan, needed, host(), policy_ref, signers_ref)\n"
    assert current.count(admission) == 1
    old_entry = current.replace(admission, "", 1)
    parser = '    current = acceptance_plan(policy, names, author)\n'
    assert current.count(parser) == 1
    isolated = current.replace(parser,
        '    os.environ["SCODE_SYNTHETIC_PLAN_PARSER_STATE"] = "parser-child"\n'
        '    print("PLAN_PARSER_NAMESPACE=" + ("calling-executor" if '
        'hasattr(sys.modules["__main__"], "preflight_plan_compatibility") else "isolated-metadata"))\n' + parser, 1)
    invalid = current.replace(parser, '    return {"supported": False}\n' + parser, 1)
    start = current.index('    # The accepted parser receives metadata, never this executor\'s namespace,\n')
    end = current.index('    if not supported:\n', start)
    # Restore the actual former in-process parser, not a fake native result.
    inprocess = current[:start] + '''    path = ".github/scripts/local_acceptance.py"
    protected = {"__file__": str(ROOT / path), "__name__": "protected_acceptance_planner"}
    exec(compile(tracked(signers_ref, path), f"{signers_ref}:{path}", "exec"), protected)
    protected["ROOT"] = ROOT
    policy = load_policy(policy_ref)
    compatible = protected.get("compatible_acceptance_plan")
    supported = (compatible(policy, names, author, plan) if callable(compatible)
                 else protected["acceptance_plan"](policy, names, author) == plan)
''' + current[end:]
    platform = load("admission_platform", SOURCE / ".github/scripts/local_acceptance.py").host()
    records = []
    for case, protected_source, candidate_source in [
        ("old-entry-late-refusal", legacy, old_entry),
        ("early-refusal", legacy, current),
        ("compatible-parser", current, current),
        ("isolated-parser", isolated, current),
        ("old-inprocess-parser", isolated, inprocess),
        ("invalid-parser-result", invalid, current),
    ]:
        directory = output / case
        work = directory / "work"
        work.mkdir(parents=True)
        commands = []

        def execute(arguments, *, expected=0):
            result = subprocess.run(arguments, cwd=work, env=environment, capture_output=True, timeout=60)
            stem = directory / f"command-{len(commands):03d}"
            stem.with_suffix(".stdout").write_bytes(result.stdout)
            stem.with_suffix(".stderr").write_bytes(result.stderr)
            commands.append({"command": arguments, "exit_code": result.returncode, "log": str(stem)})
            assert (result.returncode == 0) == (expected == 0), result.stderr.decode(errors="replace")
            return result

        def git(*arguments):
            return execute(["git", *arguments]).stdout.decode().strip()

        git("init", "--initial-branch=main")
        git("config", "user.name", "Plan admission fixture")
        git("config", "user.email", "fixture@example.invalid")
        git("config", "core.autocrlf", "false")
        key = directory / "key"
        execute(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(key)])
        for relative in [".github/local-acceptance/gate.py",
                         ".github/local-acceptance/publish.py",
                         ".github/local-acceptance/cross_platform.py",
                         ".github/workflows/local-acceptance.yml",
                         ".github/workflows/local-acceptance-workspace.yml",
                         ".github/workflows/local-acceptance-publish.yml"]:
            target = work / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(SOURCE / relative, target)
        entry = work / ".github/scripts/local_acceptance.py"
        entry.parent.mkdir(parents=True)
        entry.write_text(protected_source, encoding="utf-8")
        (work / "spec").mkdir()
        policy = {"version": 1, "repository": "fixture/plan-admission", "checks": {
            "native-workspace": {"include": ["input.txt"]}}}
        policy_path = work / "spec/local_acceptance.json"
        policy_path.write_text(json.dumps(policy), encoding="utf-8")
        (work / "spec/local_acceptance_signers.json").write_text(json.dumps({"version": 1, "signers": {
            "fixture": {"platform": platform["system"], "public_key": key.with_suffix(".pub").read_text().strip()}}}), encoding="utf-8")
        checks = work / ".github/local-acceptance/checks.py"
        checks.parent.mkdir(parents=True, exist_ok=True)
        (checks.parent / "Dockerfile").write_text("FROM scratch\n", encoding="utf-8")
        native = "from pathlib import Path;Path('native-started').write_text('executed')"
        if case in {"isolated-parser", "old-inprocess-parser"}:
            native += (';import os;assert os.environ.get("SCODE_SYNTHETIC_PLAN_PARSER_STATE") is None,'
                       '"Protected plan parser leaked parent environment"')
        checks.write_text('import sys\ndef run(name,root,output,base,execute):\n'
            ' output.mkdir(parents=True,exist_ok=True)\n'
            ' return [execute([sys.executable,"-c",' + repr(native) + '],output,output/"000.log")]\n', encoding="utf-8")
        (work / "input.txt").write_text("before", encoding="utf-8")
        (work / ".gitignore").write_text("__pycache__/\n", encoding="utf-8")
        git("add", ".")
        git("-c", "commit.gpgsign=false", "commit", "-m", "Protected plan fixture")
        base = git("rev-parse", "HEAD")
        entry.write_text(candidate_source, encoding="utf-8")
        policy["checks"]["native-workspace"]["complementary"] = "periodic"
        policy_path.write_text(json.dumps(policy), encoding="utf-8")
        (work / "input.txt").write_text("pass", encoding="utf-8")
        git("add", ".")
        git("-c", "commit.gpgsign=false", "commit", "-m", "Candidate plan fixture")
        head = git("rev-parse", "HEAD")
        git("config", "localAcceptance.signer", "fixture")
        git("config", "localAcceptance.key", str(key))
        acceptance = directory / "acceptance"
        result = execute([sys.executable, "-B", str(entry), "check", "--head", head,
            "--base", base, "--policy-ref", head, "--signers-ref", base,
            "--output", str(acceptance)], expected=1 if case in {
                "early-refusal", "old-inprocess-parser", "invalid-parser-result"} else 0)
        marker = acceptance / "native-workspace/native-started"
        if case == "old-inprocess-parser":
            assert marker.read_text() == "executed"
            assert b"Protected plan parser leaked parent environment" in result.stderr
            assert b"PLAN_PARSER_NAMESPACE=calling-executor" in result.stdout
            assert not (acceptance / "envelope.json").exists()
        elif case == "invalid-parser-result":
            assert b"Protected acceptance planner returned a non-boolean result" in result.stderr
            assert not marker.exists() and not (acceptance / "protected_profiles.py").exists()
            assert not (acceptance / "envelope.json").exists()
        elif case == "early-refusal":
            assert b"protected verifier cannot validate this candidate acceptance plan" in result.stderr
            assert not marker.exists() and not (acceptance / "protected_profiles.py").exists()
            assert not (acceptance / "envelope.json").exists()
        else:
            assert marker.read_text() == "executed" and (acceptance / "envelope.json").is_file()
            if case == "isolated-parser":
                assert b"PLAN_PARSER_NAMESPACE=isolated-metadata" in result.stdout
            verifier = directory / "protected-verifier.py"
            verifier.write_text(protected_source, encoding="utf-8")
            code = ('import importlib.util,json,sys;from pathlib import Path;'
                    's=importlib.util.spec_from_file_location("protected",sys.argv[1]);'
                    'h=importlib.util.module_from_spec(s);s.loader.exec_module(h);h.ROOT=Path.cwd();'
                    'h.verify_envelope(json.loads(Path(sys.argv[2]).read_bytes()),sys.argv[3],sys.argv[4],sys.argv[3],signers_ref=sys.argv[4])')
            verified = execute([sys.executable, "-B", "-c", code, str(verifier),
                str(acceptance / "envelope.json"), head, base], expected=1 if case == "old-entry-late-refusal" else 0)
            if case == "old-entry-late-refusal":
                assert b"Signed acceptance plan does not match the protected policy/author host" in verified.stderr
            else:
                assert b"LOCAL_ACCEPTANCE_PLAN_ADMISSION" in result.stdout
        assert not git("status", "--porcelain")
        records.append({"case": case, "native_child_executed": marker.exists(), "commands": commands})
    (output / "result.json").write_text(json.dumps(records, indent=2) + "\n", encoding="utf-8")
    return records


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
            summary["plan_admission"] = plan_admission_controls(output / "plan-admission")
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
