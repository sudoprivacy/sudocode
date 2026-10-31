"""Real Git/SSH/CLI contract updates against a loopback GitHub API fixture.

The API fixture tests protocol enforcement; it does not claim live GitHub authority.
The production protected gate never reads this fixture or an alternate API URL.
"""

import base64
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import threading
import urllib.parse


def verify_contract_update(directory, work, clean_environment):
    fixture = directory / "contract-api"
    fixture.mkdir()
    helper = work / ".github/scripts/local_acceptance.py"
    environment = dict(clean_environment)

    def run(*args, success=True):
        result = subprocess.run(list(map(str, args)), cwd=work, env=environment,
                                capture_output=True, text=True, encoding="utf-8", errors="replace")
        assert (result.returncode == 0) == success, result.stdout + result.stderr
        return result.stdout.strip() if success else result.stdout + result.stderr

    trusted = run("git", "rev-parse", "origin/main")
    trusted_runner = fixture / "trusted.py"
    trusted_runner.write_text(run("git", "show", trusted + ":.github/scripts/local_acceptance.py"))
    original_branch = run("git", "branch", "--show-current")
    origin = run("git", "remote", "get-url", "origin")
    run("git", "checkout", "-b", "authorized-contract", "origin/main")
    run("git", "remote", "add", "contract-origin", "https://github.com/acceptance/fixture.git")
    run("git", "config", f"url.{origin}.insteadOf", "https://github.com/acceptance/fixture.git")
    state = {"role": "write", "statuses": []}

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *_args):
            pass

        def reply(self, value, code=200):
            body = json.dumps(value).encode()
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self):
            path = urllib.parse.urlsplit(self.path).path
            if path == "/user":
                self.reply({"login": "fixture-maintainer"})
            elif path.endswith("/permission"):
                actor = urllib.parse.unquote(path.split("/")[-2])
                role = state["role"] if actor == "fixture-maintainer" else "write"
                self.reply({"permission": "write", "role_name": role})
            elif path.endswith("/statuses"):
                self.reply(state["statuses"])
            else:
                self.reply({"message": "Unknown fixture resource"}, 404)

        def do_POST(self):
            body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
            assert body["context"] == "Acceptance contract authorization", body
            head = self.path.rsplit("/", 1)[-1]
            exists = subprocess.run(["git", "cat-file", "-e", head + "^{commit}"], cwd=origin,
                                    env=clean_environment, capture_output=True).returncode == 0
            if not exists:
                self.reply({"message": "Commit has not been pushed"}, 422)
                return
            if state.pop("reject_next_post", False):
                self.reply({"message": "Fixture interrupts authorization after the actual push"}, 503)
                return
            status = {**body, "creator": {"login": "fixture-maintainer"}}
            state["statuses"].insert(0, status)
            self.reply(status, 201)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    client = fixture / "gh_client.py"
    client.write_text(
        'import sys, urllib.request, urllib.error\n'
        'args=sys.argv[1:]\n'
        'assert args[0] == "api", args\n'
        'method=args[args.index("--method")+1]\n'
        f'url="http://127.0.0.1:{server.server_port}/"+args[1]\n'
        'body=sys.stdin.buffer.read() if method == "POST" else None\n'
        'try:\n'
        ' with urllib.request.urlopen(urllib.request.Request(url,data=body,method=method)) as response:\n'
        '  sys.stdout.buffer.write(response.read())\n'
        'except urllib.error.HTTPError as error:\n'
        ' sys.stderr.buffer.write(error.read()); raise SystemExit(1)\n'
    )
    tools = fixture / "bin"
    tools.mkdir()
    if os.name == "nt":
        native = fixture / "native-gh"
        (native / "src").mkdir(parents=True)
        (native / "Cargo.toml").write_text('[package]\nname="gh"\nversion="0.0.0"\nedition="2021"\n[workspace]\n')
        (native / "src/main.rs").write_text(
            'fn main(){let result=std::process::Command::new(' + json.dumps(sys.executable) + ')'
            '.arg(' + json.dumps(str(client)) + ').args(std::env::args_os().skip(1)).status().unwrap();'
            'std::process::exit(result.code().unwrap_or(1));}\n'
        )
        build = fixture / "build_native.py"
        source_executor = Path(__file__).resolve().parents[2] / ".github/scripts/local_acceptance.py"
        build.write_text(
            'import importlib.util\nfrom pathlib import Path\n'
            f'spec=importlib.util.spec_from_file_location("source_executor",{str(source_executor)!r})\n'
            'executor=importlib.util.module_from_spec(spec); spec.loader.exec_module(executor)\n'
            f'executor.run_command(["cargo","build","--offline","--manifest-path",{str(native / "Cargo.toml")!r},'
            f'"--target-dir",{str(native / "target")!r}],Path({str(native)!r}),Path({str(fixture / "native-gh-build.log")!r}))\n'
        )
        # The outer guard deliberately inherits another repository's Git
        # selectors. Compile in a clean child so MSVC discovery reads our repo.
        run(sys.executable, build)
        import shutil
        shutil.copy2(native / "target/debug/gh.exe", tools / "gh.exe")
    else:
        executable = tools / "gh"
        executable.write_text(f'#!/bin/sh\nexec {shlex.quote(sys.executable)} {shlex.quote(str(client))} "$@"\n')
        executable.chmod(0o755)
    environment["PATH"] = str(tools) + os.pathsep + environment["PATH"]
    try:
        policy = work / "spec/local_acceptance.json"
        data = json.loads(policy.read_text())
        data["checks"]["added-process"] = {"include": ["**"]}
        policy.write_text(json.dumps(data))
        helper.write_text(helper.read_text().replace('    import psutil\n',
                          '    import psutil\n    print("CANDIDATE_EXECUTOR_USED", flush=True)\n', 1))
        run("git", "add", ".")
        run("git", "commit", "-m", "Add an actually executed acceptance command")
        rejected = run(sys.executable, trusted_runner, "contract-update", "--remote", "contract-origin", success=False)
        assert "maintain/admin authority" in rejected, rejected
        assert not run("git", "ls-remote", "origin", "refs/heads/authorized-contract")
        assert not state["statuses"]

        state["role"] = "maintain"
        (work / "input.txt").write_text("fail")
        run("git", "add", ".")
        run("git", "commit", "-m", "A failed real command still blocks contract authorization")
        rejected = run(sys.executable, trusted_runner, "contract-update", "--remote", "contract-origin", success=False)
        assert "Local acceptance failed" in rejected, rejected
        assert not run("git", "ls-remote", "origin", "refs/heads/authorized-contract")
        assert not state["statuses"]

        (work / "input.txt").write_text("pass")
        run("git", "add", ".")
        run("git", "commit", "-m", "Both real candidate commands pass")
        # Independent docs updates on main must not force another source stamp
        # and rebuild of an otherwise current acceptance contract.
        run("git", "checkout", "main")
        (work / "unrelated-main.md").write_text("Independent documentation update\n")
        run("git", "add", ".")
        run("git", "commit", "-m", "Advance main outside the protected contract")
        run("git", "push", "origin", "main")
        run("git", "checkout", "authorized-contract")
        run("git", "merge-base", "--is-ancestor", "origin/main", "HEAD", success=False)
        state["reject_next_post"] = True
        interrupted = run(sys.executable, trusted_runner, "contract-update", "--remote", "contract-origin", success=False)
        assert "Branch was pushed, but contract authorization is incomplete" in interrupted, interrupted
        operation = json.loads(next(line.split("=", 1)[1] for line in interrupted.splitlines()
                                    if line.startswith("LOCAL_ACCEPTANCE_OPERATION_TIMING=")
                                    and '"operation":"contract-update"' in line))
        assert operation["outcome"] == "failure" and operation["elapsed_seconds"] > 0
        assert operation["started_at"] <= operation["finished_at"]
        assert "CANDIDATE_EXECUTOR_USED" in interrupted, interrupted
        head = run("git", "rev-parse", "HEAD")
        assert run("git", "ls-remote", "origin", "refs/heads/authorized-contract").split()[0] == head
        run("git", "fetch", "origin", "+refs/notes/local-acceptance:refs/notes/local-acceptance")
        note = json.loads(run("git", "notes", "--ref=local-acceptance", "show", head))
        receipt = json.loads(base64.b64decode(note["attestations"]["fixture"]["payload"]))
        assert set(receipt["checks"]) == {"real-process", "added-process"}, receipt
        assert all(command["exit_code"] == 0 for commands in receipt["checks"].values() for command in commands)
        assert not state["statuses"]
        passed = run(sys.executable, trusted_runner, "contract-update", "--remote", "contract-origin")
        assert "complementary CI checks must still pass" in passed, passed
        assert "CANDIDATE_EXECUTOR_USED" not in passed, passed
        assert json.loads(run("git", "notes", "--ref=local-acceptance", "show", head)) == note
        assert len(state["statuses"]) == 1

        # Import only the trusted-base verifier, then use real signed notes.
        proof = fixture / "verify.py"
        proof.write_text(
            'import importlib.util\nfrom pathlib import Path\n'
            f'spec=importlib.util.spec_from_file_location("trusted",{str(trusted_runner)!r})\n'
            'module=importlib.util.module_from_spec(spec); spec.loader.exec_module(module)\n'
            f'module.ROOT=Path({str(work)!r})\n'
            f'policy=module.authorized_policy({head!r},{trusted!r},{trusted!r})\n'
            f'module.verify({head!r},{trusted!r},policy,signers_ref={trusted!r})\n'
        )
        run(sys.executable, proof)
        approved = dict(state["statuses"][0])
        for changed in [
            {"state": "failure"},
            {"description": "v1 " + "0" * 64 + " " + "1" * 64},
            {"creator": {"login": "unprivileged-writer"}},
            {"target_url": "https://example.invalid/unrelated"},
        ]:
            state["statuses"][0] = {**approved, **changed}
            run(sys.executable, proof, success=False)
        state["statuses"][0] = approved
        run(sys.executable, proof)
        # An independent protected-main change does require integration.
        run("git", "checkout", "main")
        (work / ".github/local-acceptance/Dockerfile").write_text("FROM scratch\n# independent trusted update\n")
        run("git", "add", ".")
        run("git", "commit", "-m", "Advance the trusted acceptance image")
        advanced = run("git", "rev-parse", "HEAD")
        # Model the remote maintainer's independent update in the real bare
        # fixture; production hooks and protections are never disabled.
        run("git", "push", "origin", advanced + ":refs/fixture/independent-main")
        run("git", "--git-dir", origin, "update-ref", "refs/heads/main", advanced)
        run("git", "checkout", "authorized-contract")
        stale = run(sys.executable, trusted_runner, "contract-update", "--remote", "contract-origin", success=False)
        assert "Trusted acceptance contract advanced" in stale, stale
        assert run("git", "ls-remote", "origin", "refs/heads/authorized-contract").split()[0] == head
        assert state["statuses"] == [approved]
        print("PASS: real contract-update CLI, native child failures, actual Git transport and SSH notes; "
              "loopback API rejects writer authority, revoked/stale/wrong-target authorization; "
              "trusted-base verification executes no candidate verifier and writes no test success status")
    finally:
        server.shutdown()
        server.server_close()
        thread.join()
        run("git", "checkout", original_branch)
        run("git", "--git-dir", origin, "update-ref", "refs/heads/main", trusted)
        run("git", "--git-dir", origin, "update-ref", "-d", "refs/fixture/independent-main")
        run("git", "update-ref", "refs/heads/main", trusted)
        run("git", "fetch", "origin", "+refs/heads/main:refs/remotes/origin/main")
