"""Exercise carried profiles across a branch's later heads through actual CLI processes, Git and SSH."""

import base64
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


def verify_carryover(directory, source, environment):
    fixture = directory / "carryover"
    fixture.mkdir()
    work, origin, home = fixture / "work", fixture / "origin.git", fixture / "author-home"
    env = {**environment, "HOME": str(home), "USERPROFILE": str(home)}

    def run(*args, cwd=work, success=True, error=None):
        result = subprocess.run(list(map(str, args)), cwd=cwd, capture_output=True, text=True,
                                encoding="utf-8", errors="replace", env=env)
        if (result.returncode == 0) != success:
            raise RuntimeError(f"Unexpected exit {result.returncode}: {args}\n{result.stdout}\n{result.stderr}")
        if error is not None:
            assert error in result.stderr, result.stderr
        return result.stdout.strip()

    run("git", "init", "--bare", origin, cwd=fixture)
    run("git", "init", "--initial-branch=main", work, cwd=fixture)
    run("git", "config", "user.name", "Carryover E2E")
    run("git", "config", "user.email", "carryover@example.invalid")
    run("git", "remote", "add", "origin", origin)
    helper = work / ".github/scripts/local_acceptance.py"
    helper.parent.mkdir(parents=True)
    shutil.copyfile(source / ".github/scripts/local_acceptance.py", helper)
    profiles = work / ".github/local-acceptance/checks.py"
    profiles.parent.mkdir()
    profiles.write_text(
        'import sys\n'
        'def run(name, root, output, base, execute):\n'
        '    code = "from pathlib import Path; import sys; '
        'Path(sys.argv[1]).open(\'a\').write(sys.argv[2]+chr(10)); print(sys.argv[2])"\n'
        '    return [execute([sys.executable, "-c", code, str(root.parent / "events.log"), name], '
        'root, output / "000.log")]\n'
    )
    (profiles.parent / "Dockerfile").write_text("FROM scratch\n")
    workflows = work / ".github/workflows"
    workflows.mkdir()
    for name in ["local-acceptance.yml", "local-acceptance-publish.yml"]:
        (workflows / name).write_text("# Local enforcement fixture\n")
    (work / "spec").mkdir()
    (work / "spec/local_acceptance.json").write_text(json.dumps({
        "version": 1, "repository": "acceptance/carryover-fixture", "checks": {
            "preflight": {"include": []},
            "native-workspace": {"include": ["src/*"]},
            "browser": {"include": ["web/*"]},
        },
    }))
    key = fixture / "device-key"
    run("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", key)
    system = {"win32": "windows", "darwin": "macos"}.get(sys.platform, "linux")
    (work / "spec/local_acceptance_signers.json").write_text(json.dumps({
        "version": 1, "signers": {"fixture": {
            "platform": system, "public_key": key.with_suffix(".pub").read_text().strip(),
        }},
    }))
    run("git", "config", "localAcceptance.signer", "fixture")
    run("git", "config", "localAcceptance.key", key)
    (work / ".gitignore").write_text("__pycache__/\n")
    for path in ["src/core.txt", "src/shared.txt", "web/page.txt"]:
        (work / path).parent.mkdir(exist_ok=True)
        (work / path).write_text("initial\n")
    run("git", "add", ".")
    run("git", "commit", "-m", "Protected carryover fixture")
    run("git", "push", "origin", "main")
    run("git", "checkout", "-b", "feature")

    def commit(message, files):
        for path, text in files.items():
            (work / path).write_text(text)
        run("git", "add", ".")
        run("git", "commit", "-m", message)
        return run("git", "rev-parse", "HEAD")

    def check():
        text = run(sys.executable, helper, "check", "--base", "main", "--policy-ref", "main", "--publish")
        head = run("git", "rev-parse", "HEAD")
        results = sorted((home / ".nexus/local-acceptance/results" / head).iterdir())
        return text, json.loads((results[-1] / "receipt.json").read_text())

    def events():
        path = fixture / "events.log"
        lines = path.read_text().splitlines() if path.exists() else []
        path.write_text("")
        return lines

    def verify():
        run("git", "fetch", "origin", "+refs/notes/local-acceptance:refs/notes/local-acceptance")
        return run(sys.executable, helper, "verify", "--base", "main", "--policy-ref", "main")

    def origins(receipt):
        return {group["head"]: group["profiles"] for group in receipt.get("carried", [])}

    first = commit("Change product and page", {"src/core.txt": "pr core\n", "src/extra.txt": "pr extra\n",
                                               "web/page.txt": "pr page\n"})
    _, receipt = check()
    assert events() == ["preflight", "native-workspace", "browser"] and "carried" not in receipt

    # A page-only commit selects browser, not native-workspace.
    second = commit("Change only the page", {"web/page.txt": "pr page 2\n"})
    text, receipt = check()
    assert events() == ["preflight", "browser"], text
    assert origins(receipt) == {first: ["native-workspace"]}, receipt.get("carried")
    assert "Carrying signed native-workspace from" in text, text
    verify()

    # Main changes a product file the PR never touched; a clean merge carries both
    # profiles because main's own change was accepted on main.
    run("git", "checkout", "main")
    commit("Main-only product change", {"src/shared.txt": "main shared\n"})
    run("git", "push", "origin", "main")
    run("git", "checkout", "feature")
    run("git", "merge", "--no-edit", "main")
    run("git", "rev-parse", "HEAD")
    text, receipt = check()
    assert events() == ["preflight"], text
    assert origins(receipt) == {first: ["native-workspace"], second: ["browser"]}, receipt.get("carried")
    verify()

    # A carried profile carries again from its true origin.
    third = commit("Another page-only commit", {"web/page.txt": "pr page 3\n"})
    text, receipt = check()
    assert events() == ["preflight", "browser"], text
    assert origins(receipt) == {first: ["native-workspace"]}, receipt.get("carried")
    verify()

    # Main replaces one of the PR's product changes; taking main's side is still a
    # PR change, so native-workspace (still required by src/extra.txt) runs again.
    run("git", "checkout", "main")
    commit("Main rewrites the PR's file", {"src/core.txt": "main core\n"})
    run("git", "push", "origin", "main")
    run("git", "checkout", "feature")
    run("git", "merge", "--no-edit", "main", success=False)
    run("git", "checkout", "--theirs", "src/core.txt")
    run("git", "add", "src/core.txt")
    run("git", "commit", "--no-edit")
    text, receipt = check()
    happened = events()
    assert happened == ["preflight", "native-workspace"], (happened, text)
    assert origins(receipt) == {third: ["browser"]}, receipt.get("carried")
    verify()

    # Tampered carried evidence is refused by the verifier.
    head = run("git", "rev-parse", "HEAD")
    note = json.loads(run("git", "notes", "--ref=local-acceptance", "show", head))
    envelope = note["attestations"]["fixture"]
    payload = json.loads(base64.b64decode(envelope["payload"]))
    assert payload.get("carried"), payload
    forged = dict(payload)
    forged["carried"] = [{**forged["carried"][0], "profiles": ["native-workspace"]}]
    data = fixture / "forged-receipt.json"
    data.write_text(json.dumps(forged, sort_keys=True, separators=(",", ":")))
    run("ssh-keygen", "-Y", "sign", "-f", key, "-n", "sudocode-local-acceptance", data)
    forged_envelope = {"signer": "fixture", "payload": base64.b64encode(data.read_bytes()).decode(),
                       "signature": base64.b64encode(data.with_suffix(".json.sig").read_bytes()).decode()}
    forged_note = fixture / "forged-note.json"
    forged_note.write_text(json.dumps({"version": 1, "attestations": {"fixture": forged_envelope}}))
    run("git", "notes", "--ref=local-acceptance", "add", "-f", "-F", forged_note, head)
    run(sys.executable, helper, "verify", "--base", "main", "--policy-ref", "main", success=False,
        error="require native-workspace to run again")
    print("PASS: actual signed CLI carries unselected profiles across later commits and clean main merges, "
          "chains to the true origin, reruns profiles a PR change or replaced PR change selects, and refuses forged carried evidence")


if __name__ == "__main__":
    from run import SOURCE, FIXTURE_ENV

    with tempfile.TemporaryDirectory(prefix="scode-carryover-acceptance-") as temporary:
        verify_carryover(Path(temporary), SOURCE, FIXTURE_ENV)
