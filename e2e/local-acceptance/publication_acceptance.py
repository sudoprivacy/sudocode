"""Exercise event ordering through real publisher processes, HTTP, Git and SSH notes."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import threading
import time
import urllib.parse


SOURCE = Path(__file__).resolve().parents[2]


def verify_publication(directory, work, clean_environment):
    output = directory / "publication"
    output.mkdir()
    workflow = (SOURCE / ".github/workflows/local-acceptance.yml").read_text(encoding="utf-8")
    draft = "github.event_name == 'pull_request' && github.event.pull_request.draft"
    assert "run-name: ${{ " + draft + " && 'Draft local acceptance' || 'Local acceptance' }}" in workflow
    assert "group: acceptance-${{ github.event.pull_request.number || github.ref }}-${{ " + draft + " && 'draft' || 'ready' }}" in workflow
    assert "base_sha: ${{ steps.proof.outputs.base_sha }}" in workflow
    assert "ACCEPTANCE_BASE: ${{ needs.proof.outputs.base_sha }}" in workflow
    publisher = (SOURCE / ".github/workflows/local-acceptance-publish.yml").read_text(encoding="utf-8")
    assert "if: github.event_name == 'workflow_dispatch' || ((github.event.workflow_run.event == 'pull_request' || github.event.workflow_run.event == 'merge_group') && github.event.workflow_run.conclusion != 'skipped')" in publisher

    def git(*args, cwd=work, data=None):
        return subprocess.check_output(["git", *map(str, args)], cwd=cwd,
                                       env=clean_environment, input=data, stderr=subprocess.PIPE).decode().strip()

    head, trusted = git("rev-parse", "HEAD"), git("rev-parse", "origin/main")
    origin = Path(git("remote", "get-url", "origin")).resolve()
    assert origin.is_relative_to(directory.resolve()) and git("rev-parse", "--is-bare-repository", cwd=origin) == "true"
    # The private bare origin writes the deliberately corrupted note below.
    # Supply its identity locally; clean containers have no global Git identity.
    git("config", "user.name", "Acceptance fixture", cwd=origin)
    git("config", "user.email", "acceptance@fixture.invalid", cwd=origin)
    git("update-ref", "refs/pull/1/head", head, cwd=origin)
    original_note_ref = git("rev-parse", "refs/notes/local-acceptance", cwd=origin)
    original_note = git("notes", "--ref=local-acceptance", "show", head, cwd=origin)
    verifier = output / "verifier"
    git("clone", "--quiet", "--branch", "main", origin, verifier, cwd=directory)
    git("config", f"url.{origin}.insteadOf", "https://github.com/acceptance/fixture.git", cwd=verifier)
    # Stage protected code in the verifier; candidate code is fetched/read only.
    for name in ["gate.py", "publish.py"]:
        shutil.copy2(SOURCE / ".github/local-acceptance" / name, verifier / ".github/local-acceptance" / name)
    shutil.copy2(SOURCE / ".github/scripts/check_pr_history.py", verifier / ".github/scripts/check_pr_history.py")
    pull = {"number": 1, "draft": False, "head": {"sha": head, "repo": {"full_name": "acceptance/fixture"}},
            "base": {"sha": trusted, "ref": "main", "repo": {"full_name": "acceptance/fixture"}}}
    state = {"runs": {}, "posts": [], "proof": "success", "statuses": [], "publisher_jobs": None}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def reply(self, value, status=200):
            data = json.dumps(value).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):
            path = urllib.parse.urlsplit(self.path).path
            prefix = "/repos/acceptance/fixture"
            if path == prefix + "/pulls":
                self.reply([pull])
            elif path == prefix + "/pulls/1":
                self.reply(pull)
            elif path == prefix + "/actions/workflows/local-acceptance.yml/runs":
                self.reply({"workflow_runs": list(state["runs"].values())})
            elif path.startswith(prefix + "/actions/runs/") and path.endswith("/jobs"):
                self.reply({"jobs": state["publisher_jobs"] if state["publisher_jobs"] is not None else
                            [{"name": "Verify local acceptance", "conclusion": state["proof"]}]})
            elif path.startswith(prefix + "/actions/runs/"):
                self.reply(state["runs"][int(path.rsplit("/", 1)[-1])])
            elif path.endswith("/statuses"):
                self.reply(state["statuses"])
            else:
                self.reply({"message": "Unknown fixture resource", "path": path}, 404)

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            assert self.path == "/repos/acceptance/fixture/statuses/" + head
            assert body["context"] == "Local acceptance"
            state["posts"].append(body)
            self.reply(body)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    bootstrap = output / "publisher-child.py"
    bootstrap.write_text(
        'import json,os,runpy,sys,urllib.request\n'
        'with open(os.devnull) as probe:\n'
        ' print("PUBLICATION_FILE_ENCODING="+json.dumps({"encoding":probe.encoding,"utf8_mode":sys.flags.utf8_mode}),flush=True)\n'
        'original=urllib.request.urlopen\n'
        'def loopback(request,*args,**kwargs):\n'
        ' assert request.full_url.startswith("https://api.github.com/")\n'
        ' url=sys.argv[1]+request.full_url[len("https://api.github.com"):]\n'
        ' mapped=urllib.request.Request(url,data=request.data,headers=dict(request.header_items()),method=request.get_method())\n'
        ' return original(mapped,*args,**kwargs)\n'
        'urllib.request.urlopen=loopback\n'
        'runpy.run_path(sys.argv[2],run_name="__main__")\n', encoding="utf-8")

    def run(identifier, *, title="Local acceptance", conclusion="success", status="completed", attempt=1):
        return {"id": identifier, "path": ".github/workflows/local-acceptance.yml", "status": status,
                "conclusion": conclusion, "display_title": title, "head_sha": head,
                "event": "pull_request", "run_attempt": attempt}

    cases = []

    def invoke(label, source, runs, *, expected=None, success=True, proof="success", attempt=1,
               gate_only=False, verified_base=None, merge_group=False, statuses=None,
               publisher_jobs=None, refusal=None):
        state.update(runs={item["id"]: item for item in runs}, posts=[], proof=proof,
                     statuses=statuses or [], publisher_jobs=publisher_jobs)
        event = output / (label + ".json")
        payload = ({"pull_request": pull} if gate_only else
                   {"workflow_run": {"id": source, "run_attempt": attempt}})
        if merge_group:
            payload = {"merge_group": {"head_sha": head, "base_sha": trusted}}
        payload["repository"] = {"full_name": "acceptance/fixture", "description": "Metadata ❯ 中文"}
        event.write_text(json.dumps(payload, ensure_ascii=False), encoding="utf-8")
        result = subprocess.run([sys.executable, str(bootstrap), f"http://127.0.0.1:{server.server_port}",
                                 str(verifier / ".github/local-acceptance" / ("gate.py" if gate_only or merge_group else "publish.py"))], cwd=verifier,
            env={**clean_environment, "GITHUB_REPOSITORY": "acceptance/fixture", "GITHUB_EVENT_PATH": str(event),
                 "GITHUB_OUTPUT": str(output / (label + ".outputs")), "GITHUB_RUN_ID": "300",
                 "PYTHONUTF8": "0", "PYTHONCOERCECLOCALE": "0", "LC_ALL": "C", "LANG": "C",
                 "GH_TOKEN": "fixture-no-real-token"}, capture_output=True)
        (output / (label + ".stdout.log")).write_bytes(result.stdout)
        (output / (label + ".stderr.log")).write_bytes(result.stderr)
        encodings = [json.loads(line.split("=", 1)[1]) for line in result.stdout.decode().splitlines()
                     if line.startswith("PUBLICATION_FILE_ENCODING=")]
        assert len(encodings) == 1 and encodings[0]["utf8_mode"] == 0, encodings
        if os.name != "nt":
            # POSIX C supplies a real non-UTF-8 default; Windows uses its ANSI codepage.
            assert encodings[0]["encoding"].lower().replace("-", "") not in {"utf8", "cp65001"}, encodings
        print("PUBLICATION_ENV_CASE=" + json.dumps({"case": label, **encodings[0]}), flush=True)
        assert (result.returncode == 0) == success, result.stdout.decode() + result.stderr.decode()
        if refusal is not None:
            assert refusal in result.stderr.decode(), result.stderr.decode()
        assert [item["state"] for item in state["posts"]] == ([] if expected is None else [expected]), state
        if verified_base is not None:
            outputs = (output / (label + ".outputs")).read_text(encoding="utf-8")
            assert f"base_sha={verified_base}\n" in outputs, outputs
        cases.append({"case": label, "exit_code": result.returncode, "published": expected})
        print("PUBLICATION_E2E_CASE=" + json.dumps(cases[-1]), flush=True)

    try:
        invoke("ready-before-newer-queued-draft", 100,
               [run(100), run(101, title="Draft local acceptance", conclusion=None, status="queued")], expected="success")
        invoke("late-draft-publishes-nothing", 101, [run(100), run(101, title="Draft local acceptance", conclusion="skipped")])
        invoke("legacy-skipped-cannot-mask-ready", 100, [run(100), run(101, title="Old draft title", conclusion="skipped")], expected="success")
        invoke("newer-ready-still-owns-head", 100, [run(100), run(102, conclusion=None, status="in_progress")])
        invoke("failed-ready-still-fails", 102, [run(102, conclusion="failure")], expected="failure", success=False, proof="failure")
        invoke("old-success-cannot-replace-ready-failure", 100, [run(100), run(102, conclusion="failure")])
        invoke("old-attempt-ignored", 102, [run(102, attempt=2)])
        tampered = json.loads(original_note)
        tampered["attestations"]["fixture"]["signature"] = "YmFkIHNpZ25hdHVyZQ=="
        git("notes", "--ref=local-acceptance", "add", "-f", "-F", "-", head,
            cwd=origin, data=json.dumps(tampered).encode())
        invoke("invalid-real-ssh-note-refused", 103, [run(103)], expected="failure", success=False)
        refusal = ((output / "invalid-real-ssh-note-refused.stdout.log").read_text(encoding="utf-8")
                   + (output / "invalid-real-ssh-note-refused.stderr.log").read_text(encoding="utf-8"))
        assert "Invalid SSH acceptance signature" in refusal, refusal
        git("update-ref", "refs/notes/local-acceptance", original_note_ref, cwd=origin)
        invoke("restored-real-note-passes", 104, [run(104)], expected="success")
        publisher_run = {"id": 300, "path": ".github/workflows/local-acceptance-publish.yml",
                         "event": "workflow_run", "head_sha": trusted, "run_attempt": 2,
                         "status": "completed", "conclusion": "failure"}
        published = {"context": "Local acceptance", "state": "success",
                     "creator": {"login": "github-actions[bot]"},
                     "target_url": "https://github.com/acceptance/fixture/actions/runs/300",
                     "created_at": "2026-11-05T16:42:17Z"}
        successful_job = {"id": 302, "run_id": 300, "run_attempt": 2, "head_sha": trusted,
                          "status": "completed", "conclusion": "success",
                          "started_at": "2026-11-05T16:41:59Z", "completed_at": "2026-11-05T16:42:23Z"}
        old_job = {**successful_job, "id": 301, "run_attempt": 1, "conclusion": "failure"}

        def queue(label, *, run_changes=None, job_changes=None, status_changes=None,
                  jobs=None, success=False, refusal="Acceptance publisher attempt"):
            invoke(label, 300, [{**publisher_run, **(run_changes or {})}], merge_group=True,
                   statuses=[{**published, **(status_changes or {})}],
                   publisher_jobs=(jobs if jobs is not None else [old_job, {**successful_job, **(job_changes or {})}]),
                   success=success, refusal=None if success else refusal)

        queue("queue-stale-failed-aggregate-current-jobs-success", success=True)
        queue("queue-stale-queued-aggregate-current-jobs-success",
              run_changes={"status": "queued", "conclusion": None}, success=True)
        queue("queue-first-attempt-success", run_changes={"run_attempt": 1, "conclusion": "success"}, success=True)
        queue("queue-first-attempt-publishing", run_changes={"run_attempt": 1, "conclusion": None}, success=True)
        queue("queue-current-failed-job-refused", job_changes={"conclusion": "failure"})
        queue("queue-success-aggregate-failed-current-job-refused",
              run_changes={"conclusion": "success"}, job_changes={"conclusion": "failure"})
        queue("queue-another-current-job-failed-refused",
              jobs=[successful_job, {**successful_job, "id": 303, "conclusion": "failure"}])
        queue("queue-current-pending-job-refused", job_changes={"status": "in_progress", "conclusion": None})
        queue("queue-current-jobs-missing-refused", jobs=[old_job])
        queue("queue-newer-job-attempt-refused", jobs=[successful_job, {**successful_job, "run_attempt": 3}])
        queue("queue-job-wrong-source-refused", job_changes={"head_sha": head})
        queue("queue-job-wrong-run-refused", job_changes={"run_id": 301})
        queue("queue-old-status-refused", status_changes={"created_at": "2026-11-05T16:39:22Z"},
              refusal="Acceptance status does not belong")
        queue("queue-malformed-status-time-refused", status_changes={"created_at": "invalid"},
              refusal="Acceptance status does not belong")
        queue("queue-status-after-job-refused", status_changes={"created_at": "2026-11-05T16:42:24Z"},
              refusal="Acceptance status does not belong")
        queue("queue-wrong-workflow-refused", run_changes={"path": ".github/workflows/other.yml"},
              refusal="not produced by the trusted publisher")
        queue("queue-wrong-event-refused", run_changes={"event": "push"},
              refusal="not produced by the trusted publisher")
        queue("queue-first-attempt-failed-refused", run_changes={"run_attempt": 1},
              refusal="not produced by the trusted publisher")
        # Model a protected main that has incorporated an ancestor of this PR,
        # while the PR API still reports its old base. Run/sign real commands
        # against that actual new base; do not edit or re-sign an old receipt.
        advanced = git("rev-parse", head + "^")
        assert advanced != trusted and git("merge-base", advanced, head) == advanced
        git("update-ref", "refs/heads/main", advanced, cwd=origin)
        git("fetch", "--quiet", "origin", "main", cwd=work)
        git("checkout", "--quiet", "--detach", advanced, cwd=verifier)
        result = subprocess.run([sys.executable, str(work / ".github/scripts/local_acceptance.py"),
                                 "check", "--head", head, "--base", advanced, "--policy-ref", advanced,
                                 "--output", str(output / "advanced-base-acceptance"), "--publish", "--remote", "origin"],
                                cwd=work, env=clean_environment, capture_output=True)
        (output / "advanced-base-acceptance.stdout.log").write_bytes(result.stdout)
        (output / "advanced-base-acceptance.stderr.log").write_bytes(result.stderr)
        assert result.returncode == 0, result.stdout.decode() + result.stderr.decode()
        receipt = json.loads((output / "advanced-base-acceptance/receipt.json").read_text(encoding="utf-8"))
        assert receipt["base"] == advanced and receipt["head"] == head
        invoke("stale-api-base-direct-gate", 105, [], gate_only=True, verified_base=advanced)
        invoke("stale-api-base-publisher", 105, [run(105)], expected="success", verified_base=advanced)
        git("update-ref", "refs/notes/local-acceptance", original_note_ref, cwd=origin)
        invoke("old-real-base-receipt-refused", 106, [run(106)], expected="failure", success=False)
        refusal = ((output / "old-real-base-receipt-refused.stdout.log").read_text(encoding="utf-8")
                   + (output / "old-real-base-receipt-refused.stderr.log").read_text(encoding="utf-8"))
        assert "Acceptance used a different PR base" in refusal, refusal
    finally:
        git("update-ref", "refs/notes/local-acceptance", original_note_ref, cwd=origin)
        git("update-ref", "refs/notes/local-acceptance", original_note_ref, cwd=work)
        git("update-ref", "refs/heads/main", trusted, cwd=origin)
        git("fetch", "--quiet", "origin", "main", cwd=work)
        server.shutdown()
        thread.join()
        server.server_close()
    verify_note_reference_race(output, clean_environment)
    cases.append({"case": "real-notes-reference-race-and-stable-refusal", "exit_code": 0})
    (output / "result.json").write_text(json.dumps({"status": "PASS", "cases": cases,
        "scope": "Actual publisher child processes and loopback HTTP API with real Git fetches and SSH notes. Candidate code is not run; no live GitHub authority or product journey credit."}, indent=2) + "\n", encoding="utf-8")
    print("PASS: draft/ready ordering, frozen protected base, current ownership and real SSH publisher refusal")


def verify_note_reference_race(directory, clean_environment):
    """Advance a real bare ref after push negotiation but before its transaction."""
    output = directory / "notes-reference-race"
    output.mkdir()
    origin, work = output / "origin.git", output / "work"
    git_environment = {**clean_environment, "LC_ALL": "C", "LANG": "C"}

    def git(*args, cwd=work, data=None):
        return subprocess.check_output(["git", *map(str, args)], cwd=cwd,
            env=git_environment, input=data, stderr=subprocess.PIPE).decode().strip()

    git("init", "--quiet", "--bare", origin, cwd=output)
    git("init", "--quiet", "--initial-branch=main", work, cwd=output)
    git("config", "user.name", "Notes race fixture")
    git("config", "user.email", "notes-race@fixture.invalid")
    git("config", "commit.gpgsign", "false")
    (work / "input").write_text("baseline\n", encoding="utf-8")
    git("add", "input")
    git("commit", "--quiet", "-m", "Baseline")
    baseline_head = git("rev-parse", "HEAD")
    (work / "input").write_text("candidate\n", encoding="utf-8")
    git("commit", "--quiet", "-am", "Candidate")
    head = git("rev-parse", "HEAD")
    git("remote", "add", "origin", origin)
    git("push", "--quiet", "origin", "HEAD:refs/heads/main")
    notes_ref = "refs/notes/local-acceptance"
    git("notes", "--ref=" + notes_ref, "add", "-m", "baseline note", baseline_head)
    previous = git("rev-parse", notes_ref)
    git("push", "--quiet", "origin", notes_ref)
    prepared = "refs/notes/prepared-race"
    git("update-ref", prepared, previous)
    git("notes", "--ref=" + prepared, "add", "-f", "-m", "concurrent winner", baseline_head)
    other = {"signer": "concurrent-fixture", "payload": "opaque concurrent fixture data"}
    git("notes", "--ref=" + prepared, "add", "-m",
        json.dumps({"version": 1, "attestations": {other["signer"]: other}}), head)
    concurrent = git("rev-parse", prepared)
    git("push", "--quiet", "origin", prepared)

    # The hook only holds the transaction. The parent advances the actual bare
    # ref after receive-pack advertised its old value; no Git result is mocked.
    hook = output / "receive-barrier.py"
    hook.write_text(
        'import json,os,sys,time\nfrom pathlib import Path\n'
        'root=Path(os.environ["SCODE_NOTES_RACE_DIRECTORY"])\n'
        'updates=sys.stdin.read().splitlines()\n'
        'if any(row.endswith(" refs/notes/local-acceptance") for row in updates):\n'
        ' try: descriptor=os.open(root/"claimed",os.O_CREAT|os.O_EXCL|os.O_WRONLY,0o600)\n'
        ' except FileExistsError: pass\n'
        ' else:\n'
        '  os.close(descriptor)\n'
        '  ready=root/"ready.tmp"\n'
        '  ready.write_text(json.dumps(updates),encoding="utf-8");ready.replace(root/"ready.json")\n'
        '  deadline=time.monotonic()+90\n'
        '  while not (root/"release").exists():\n'
        '   if time.monotonic()>=deadline: raise SystemExit("receive barrier expired")\n'
        '   time.sleep(0.01)\n', encoding="utf-8")
    receive = origin / "hooks/pre-receive"
    receive.write_text('#!/bin/sh\nexec "$SCODE_NOTES_RACE_PYTHON" "$SCODE_NOTES_RACE_HOOK"\n',
                       encoding="utf-8")
    receive.chmod(0o755)
    git("config", "core.hooksPath", origin / "hooks", cwd=origin)
    bootstrap = output / "publisher.py"
    bootstrap.write_text(
        'import importlib.util,json,sys\nfrom pathlib import Path\n'
        'spec=importlib.util.spec_from_file_location("actual_notes_publisher",sys.argv[1])\n'
        'module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)\n'
        'module.ROOT=Path(sys.argv[2]);original=module.git\n'
        'def observe(*args,**kwargs):\n'
        ' result=original(*args,**kwargs)\n'
        ' if args[0]=="push":\n'
        '  with Path(sys.argv[4]).open("a",encoding="utf-8") as stream:\n'
        '   stream.write(json.dumps({"args":args,"exit_code":result.returncode,"stderr":result.stderr.decode(errors="replace")})+"\\n")\n'
        ' return result\n'
        'module.git=observe\n'
        'module.publish(sys.argv[3],{"signer":"race-fixture","payload":"opaque fixture data; not an acceptance attestation"},"origin")\n',
        encoding="utf-8")
    environment = {**git_environment,
        "SCODE_NOTES_RACE_DIRECTORY": str(output),
        "SCODE_NOTES_RACE_PYTHON": Path(sys.executable).as_posix(),
        "SCODE_NOTES_RACE_HOOK": hook.as_posix()}
    process = subprocess.Popen([sys.executable, str(bootstrap),
        str(SOURCE / ".github/scripts/local_acceptance.py"), str(work), head,
        str(output / "actual-pushes.jsonl")], cwd=work, env=environment,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        deadline = time.monotonic() + 90
        while not (output / "ready.json").exists():
            assert process.poll() is None, "Publisher exited before the real receive transaction"
            assert time.monotonic() < deadline, "Receive transaction was not observed"
            time.sleep(0.01)
        updates = json.loads((output / "ready.json").read_text(encoding="utf-8"))
        assert len(updates) == 1 and updates[0].split()[0] == previous, updates
        git("update-ref", notes_ref, concurrent, previous, cwd=origin)
        (output / "release").touch()
        stdout, stderr = process.communicate(timeout=90)
    finally:
        (output / "release").touch()
        if process.poll() is None:
            process.kill()
            process.communicate()
    (output / "publisher.stdout.log").write_bytes(stdout)
    (output / "publisher.stderr.log").write_bytes(stderr)
    pushes = [json.loads(row) for row in
              (output / "actual-pushes.jsonl").read_text(encoding="utf-8").splitlines()]
    assert pushes and pushes[0]["exit_code"] != 0, pushes
    assert "cannot lock ref 'refs/notes/local-acceptance'" in pushes[0]["stderr"], pushes
    assert process.returncode == 0, stderr.decode(errors="replace")
    assert len(pushes) == 2 and pushes[1]["exit_code"] == 0, pushes
    assert git("notes", "--ref=" + notes_ref, "show", baseline_head, cwd=origin) == "concurrent winner"
    published = json.loads(git("notes", "--ref=" + notes_ref, "show", head, cwd=origin))
    assert published["attestations"]["race-fixture"]["payload"].startswith("opaque fixture data")
    assert published["attestations"][other["signer"]] == other
    # An unrelated push rejection with the same remote tip is not concurrency
    # and must fail after one attempt rather than masking a permission failure.
    final_tip = git("rev-parse", notes_ref, cwd=origin)
    (work / "input").write_text("refused\n", encoding="utf-8")
    git("commit", "--quiet", "-am", "Refused publisher")
    refused_head = git("rev-parse", "HEAD")
    receive.write_text('#!/bin/sh\nprintf "publication denied by fixture\\n" >&2\nexit 1\n',
                       encoding="utf-8")
    rejected = subprocess.run([sys.executable, str(bootstrap),
        str(SOURCE / ".github/scripts/local_acceptance.py"), str(work), refused_head,
        str(output / "refused-pushes.jsonl")], cwd=work, env=environment,
        capture_output=True, timeout=90)
    (output / "refused.stdout.log").write_bytes(rejected.stdout)
    (output / "refused.stderr.log").write_bytes(rejected.stderr)
    refusals = [json.loads(row) for row in
               (output / "refused-pushes.jsonl").read_text(encoding="utf-8").splitlines()]
    assert rejected.returncode != 0 and len(refusals) == 1, refusals
    assert "publication denied by fixture" in refusals[0]["stderr"], refusals
    assert git("rev-parse", notes_ref, cwd=origin) == final_tip
    (output / "result.json").write_text(json.dumps({"status": "PASS", "head": head,
        "before": previous, "concurrent": concurrent, "pushes": pushes, "stable_refusals": refusals,
        "scope": "Real Git receive-pack ref race and note preservation; no acceptance attestation or product test credit."},
        indent=2) + "\n", encoding="utf-8")
    print("PASS: real receive transaction race preserves concurrent notes and publishes the intended note")
