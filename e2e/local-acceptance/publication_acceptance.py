"""Exercise event ordering through real publisher processes, HTTP, Git and SSH notes."""

from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import shutil
import subprocess
import sys
import threading
import urllib.parse


SOURCE = Path(__file__).resolve().parents[2]


def verify_publication(directory, work, clean_environment):
    output = directory / "publication"
    output.mkdir()
    workflow = (SOURCE / ".github/workflows/local-acceptance.yml").read_text(encoding="utf-8")
    draft = "github.event_name == 'pull_request' && github.event.pull_request.draft"
    assert "run-name: ${{ " + draft + " && 'Draft local acceptance' || 'Local acceptance' }}" in workflow
    assert "group: acceptance-${{ github.event.pull_request.number || github.ref }}-${{ " + draft + " && 'draft' || 'ready' }}" in workflow
    publisher = (SOURCE / ".github/workflows/local-acceptance-publish.yml").read_text(encoding="utf-8")
    assert "if: (github.event.workflow_run.event == 'pull_request' || github.event.workflow_run.event == 'merge_group') && github.event.workflow_run.conclusion != 'skipped'" in publisher

    def git(*args, cwd=work, data=None):
        return subprocess.check_output(["git", *map(str, args)], cwd=cwd,
                                       env=clean_environment, input=data, stderr=subprocess.PIPE).decode().strip()

    head, trusted = git("rev-parse", "HEAD"), git("rev-parse", "origin/main")
    origin = Path(git("remote", "get-url", "origin")).resolve()
    assert origin.is_relative_to(directory.resolve()) and git("rev-parse", "--is-bare-repository", cwd=origin) == "true"
    git("update-ref", "refs/pull/1/head", head, cwd=origin)
    original_note_ref = git("rev-parse", "refs/notes/local-acceptance", cwd=origin)
    original_note = git("notes", "--ref=local-acceptance", "show", head, cwd=origin)
    verifier = output / "verifier"
    git("clone", "--quiet", "--branch", "main", origin, verifier, cwd=directory)
    git("config", f"url.{origin}.insteadOf", "https://github.com/acceptance/fixture.git", cwd=verifier)
    # Stage protected code in the verifier; candidate code is fetched/read only.
    for name in ["gate.py", "publish.py"]:
        shutil.copy2(SOURCE / ".github/local-acceptance" / name, verifier / ".github/local-acceptance" / name)
    pull = {"number": 1, "draft": False, "head": {"sha": head, "repo": {"full_name": "acceptance/fixture"}},
            "base": {"sha": trusted, "ref": "main", "repo": {"full_name": "acceptance/fixture"}}}
    state = {"runs": {}, "posts": [], "proof": "success"}

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
                self.reply({"jobs": [{"name": "Verify local acceptance", "conclusion": state["proof"]}]})
            elif path.startswith(prefix + "/actions/runs/"):
                self.reply(state["runs"][int(path.rsplit("/", 1)[-1])])
            elif path.endswith("/statuses"):
                self.reply([])
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
        'import runpy,sys,urllib.request\n'
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

    def invoke(label, source, runs, *, expected=None, success=True, proof="success", attempt=1):
        state.update(runs={item["id"]: item for item in runs}, posts=[], proof=proof)
        event = output / (label + ".json")
        event.write_text(json.dumps({"workflow_run": {"id": source, "run_attempt": attempt}}))
        result = subprocess.run([sys.executable, str(bootstrap), f"http://127.0.0.1:{server.server_port}",
                                 str(verifier / ".github/local-acceptance/publish.py")], cwd=verifier,
            env={**clean_environment, "GITHUB_REPOSITORY": "acceptance/fixture", "GITHUB_EVENT_PATH": str(event),
                 "GITHUB_OUTPUT": str(output / (label + ".outputs")), "GITHUB_RUN_ID": "300",
                 "GH_TOKEN": "fixture-no-real-token"}, capture_output=True)
        (output / (label + ".stdout.log")).write_bytes(result.stdout)
        (output / (label + ".stderr.log")).write_bytes(result.stderr)
        assert (result.returncode == 0) == success, result.stdout.decode() + result.stderr.decode()
        assert [item["state"] for item in state["posts"]] == ([] if expected is None else [expected]), state
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
    finally:
        git("update-ref", "refs/notes/local-acceptance", original_note_ref, cwd=origin)
        server.shutdown()
        thread.join()
        server.server_close()
    (output / "result.json").write_text(json.dumps({"status": "PASS", "cases": cases,
        "scope": "Actual publisher child processes and loopback HTTP API with real Git fetches and SSH notes. Candidate code is not run; no live GitHub authority or product journey credit."}, indent=2) + "\n", encoding="utf-8")
    print("PASS: draft/ready ordering, current ownership and real SSH publisher refusal")
