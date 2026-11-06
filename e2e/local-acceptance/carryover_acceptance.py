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

    def check(policy="main"):
        text = run(sys.executable, helper, "check", "--base", "main", "--policy-ref", policy, "--publish")
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

    def replace_signed_note(head, payload, label):
        data = fixture / (label + "-receipt.json")
        data.write_text(json.dumps(payload, sort_keys=True, separators=(",", ":")))
        run("ssh-keygen", "-Y", "sign", "-f", key, "-n", "sudocode-local-acceptance", data)
        envelope = {"signer": "fixture", "payload": base64.b64encode(data.read_bytes()).decode(),
                    "signature": base64.b64encode(data.with_suffix(".json.sig").read_bytes()).decode()}
        note = fixture / (label + "-note.json")
        note.write_text(json.dumps({"version": 1, "attestations": {"fixture": envelope}}))
        run("git", "notes", "--ref=local-acceptance", "add", "-f", "-F", note, head)

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
    replace_signed_note(head, forged, "source-selected")
    run(sys.executable, helper, "verify", "--base", "main", "--policy-ref", "main", success=False,
        error="require native-workspace to run again")

    # A candidate recipe is not protected authority. The old managed child
    # passes locally but fails publication here; both boundaries must agree.
    run("git", "checkout", "-b", "contract-source", "main")
    unmerged_policy = commit("Unmerged candidate command policy", {
        ".github/local-acceptance/checks.py": profiles.read_text() + "\n# Private candidate policy\n",
        "src/core.txt": "candidate contract input\n",
    })
    _, receipt = check(unmerged_policy)
    assert events() == ["preflight", "native-workspace"] and "carried" not in receipt
    candidate = commit("Follow up without changing native inputs", {"notes.txt": "first follow-up\n"})
    boundary = (
        'import importlib.util,sys\n'
        'spec=importlib.util.spec_from_file_location("author_boundary",sys.argv[2])\n'
        'module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)\n'
        'if sys.argv[1] in {"verify","verify-local"}:\n'
        ' if sys.argv[1]=="verify":\n'
        '  module.git("fetch","--no-tags","origin","+refs/notes/local-acceptance:refs/notes/local-acceptance")\n'
        ' module.verify(sys.argv[3],"main",sys.argv[3],signers_ref="main")\n'
        'else:\n'
        ' module.run_candidate_checks(sys.argv[3],"main","origin",sys.argv[4])\n'
    )
    bound_output = fixture / "authority-bound-output"
    text = run(sys.executable, "-c", boundary, "check", helper, candidate, bound_output)
    receipt = json.loads((bound_output / "receipt.json").read_text())
    assert events() == ["preflight", "native-workspace"], text
    assert "carried" not in receipt, receipt
    run(sys.executable, "-c", boundary, "verify", helper, candidate)
    exact_output = fixture / "authority-bound-exact-source-resume"
    run(sys.executable, "-c", boundary, "check", helper, candidate, exact_output)
    resumed = json.loads((exact_output / "receipt.json").read_text())
    assert events() == [], resumed
    assert set(resumed["resumed_from"]["profiles"]) == {"preflight", "native-workspace"}
    run(sys.executable, "-c", boundary, "verify", helper, candidate)
    # The installed protected parent can still be older than this candidate.
    # Its child argv lacks --signers-ref; the exact candidate/base pairing must
    # enforce the same authority without replacing that installed parent.
    candidate = commit("Use the legacy managed child arguments", {"notes.txt": "second follow-up\n"})
    text, receipt = check(candidate)
    assert events() == ["preflight", "native-workspace"], text
    assert "carried" not in receipt, receipt
    run(sys.executable, "-c", boundary, "verify", helper, candidate)

    # Protected origin authority does not excuse a changed candidate recipe.
    run("git", "checkout", "-b", "candidate-recipe", "main")
    recipe_origin = commit("Protected-policy native execution", {"src/core.txt": "recipe input\n"})
    _, origin_receipt = check()
    assert events() == ["preflight", "native-workspace"]
    run("git", "fetch", "origin", "+refs/notes/local-acceptance:refs/notes/local-acceptance")
    origin_note = json.loads(run("git", "notes", "--ref=local-acceptance", "show", recipe_origin))
    recipe = profiles.read_text().replace("print(sys.argv[2])", "print('candidate recipe: ' + sys.argv[2])")
    assert recipe != profiles.read_text()
    candidate = commit("Change only the candidate command recipe", {".github/local-acceptance/checks.py": recipe})
    recipe_output = fixture / "candidate-recipe-output"
    run(sys.executable, "-c", boundary, "check", helper, candidate, recipe_output)
    receipt = json.loads((recipe_output / "receipt.json").read_text())
    assert events() == ["preflight", "native-workspace"] and "carried" not in receipt
    assert "candidate recipe: native-workspace" in (recipe_output / "native-workspace/000.log").read_text()
    forged = {**receipt, "checks": {**receipt["checks"],
                                   "native-workspace": origin_receipt["checks"]["native-workspace"]},
              "carried": [{"head": recipe_origin, "profiles": ["native-workspace"],
                           "envelope": origin_note["attestations"]["fixture"]}]}
    replace_signed_note(candidate, forged, "candidate-recipe")
    run(sys.executable, "-c", boundary, "verify-local", helper, candidate, success=False,
        error="require native-workspace to run again")
    print("PASS: actual signed CLI carries unselected profiles across later commits and clean main merges, "
          "chains to the true origin, reruns selected profiles, refuses forged/unmerged-policy evidence, "
          "and binds the managed child admission to the actual protected publication authority")


if __name__ == "__main__":
    from run import SOURCE, FIXTURE_ENV

    with tempfile.TemporaryDirectory(prefix="scode-carryover-acceptance-") as temporary:
        verify_carryover(Path(temporary), SOURCE, FIXTURE_ENV)
