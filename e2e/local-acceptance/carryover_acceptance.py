"""Exercise carried profiles across a branch's later heads through actual CLI processes, Git and SSH."""

import base64
import hashlib
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

    def replace_signed_note(head, payload, label, envelopes=None):
        data = fixture / (label + "-receipt.json")
        data.write_text(json.dumps(payload, sort_keys=True, separators=(",", ":")))
        run("ssh-keygen", "-Y", "sign", "-f", key, "-n", "sudocode-local-acceptance", data)
        envelope = {"signer": "fixture", "payload": base64.b64encode(data.read_bytes()).decode(),
                    "signature": base64.b64encode(data.with_suffix(".json.sig").read_bytes()).decode()}
        note = fixture / (label + "-note.json")
        note.write_text(json.dumps({"version": 1, "attestations": {"fixture": envelope},
                                    **({"envelopes": envelopes} if envelopes else {})}))
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
    replace_signed_note(head, forged, "source-selected", note.get("envelopes"))
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
    origin_envelope = origin_note["attestations"]["fixture"]
    reference = hashlib.sha256(json.dumps(origin_envelope, sort_keys=True, separators=(",", ":")).encode()).hexdigest()
    forged = {**receipt, "version": 2, "checks": {**receipt["checks"],
                                                "native-workspace": origin_receipt["checks"]["native-workspace"]},
              "carried": [{"head": recipe_origin, "profiles": ["native-workspace"], "envelope_sha256": reference}]}
    replace_signed_note(candidate, forged, "candidate-recipe", {reference: origin_envelope})
    run(sys.executable, "-c", boundary, "verify-local", helper, candidate, success=False,
        error="require native-workspace to run again")
    print("PASS: actual signed CLI carries unselected profiles across later commits and clean main merges, "
          "chains to the true origin, reruns selected profiles, refuses forged/unmerged-policy evidence, "
          "and binds the managed child admission to the actual protected publication authority")
    verify_bounded_note(directory, source, environment)


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode()


def produced_workspace_records(source, evidence):
    """Produce one native-workspace profile with the real executor and compiler-cache session.

    A process-table double reports 3,183 compiler children, 3,160 of them under
    one cache session spanning five commands, as a full workspace build does.
    The commands, logs and records are real; only the children are doubles.
    """
    import importlib.util
    import os
    import types

    def load(name, path):
        spec = importlib.util.spec_from_file_location(name, path)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module

    helper = load("bounded_note_executor", source / ".github/scripts/local_acceptance.py")
    cache = load("bounded_note_cache", source / ".github/local-acceptance/compiler_cache.py")
    upcoming, served, identities = [[]], {}, iter(range(200000, 300000))

    class Child:
        def __init__(self, name):
            self.pid, self.name, self.priority = next(identities), name, 0

        def exe(self):
            return "/fixture/toolchain/bin/" + self.name

        def nice(self, value=None):
            if value is None:
                return self.priority
            self.priority = value

    class Process:
        # Each new command observes the compiler children prepared for it.
        def __init__(self, pid):
            self.pid = pid

        def children(self, recursive=False):
            if self.pid not in served:
                served.clear()
                served[self.pid], upcoming[0] = upcoming[0], []
            return served[self.pid]

    table = types.ModuleType("psutil")
    table.NoSuchProcess = type("NoSuchProcess", (Exception,), {})
    table.AccessDenied = type("AccessDenied", (Exception,), {})
    table.BELOW_NORMAL_PRIORITY_CLASS = 16384
    table.Process = Process
    table.virtual_memory = lambda: types.SimpleNamespace(total=64 * 1024 ** 3)
    names = ["rustc"] * 7 + ["cc", "cc1", "ld"]
    tool = {"version": cache.VERSION, "target": "fixture-target", "filename": "sccache-fixture.tar.gz",
            "sha256": "0" * 64, "member": "sccache-fixture/sccache", "archive": str(evidence / "sccache-fixture.tar.gz"),
            "executable": sys.executable, "executable_sha256": "1" * 64}
    stats = json.dumps({"stats": {"compile_requests": 3160, "requests_executed": 3160,
                                  "cache_hits": {"counts": {"Rust": 2871, "C/C++": 116}},
                                  "cache_misses": {"counts": {"Rust": 162, "C/C++": 11}}},
                        "cache_location": "Local disk", "max_cache_size": 2 * 1024 ** 3})
    profile = evidence / "native-workspace"
    # Command sizes follow the field breakdown of an actual workspace receipt.
    children = {3: 14, 9: 2000, 10: 9, 11: 22, 12: 600, 13: 400, 14: 138}
    padding = {3: 24000, 4: 9300, 9: 2200}
    ambient = dict(os.environ)
    real = sys.modules.get("psutil")
    try:
        for name in list(os.environ):
            if name.upper().startswith("SCCACHE_") or name.upper() in {"RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"}:
                os.environ.pop(name)
        sys.modules["psutil"] = table
        cache.compiler_frontend = lambda command: True
        session = cache.Session(tool, evidence / "compiler-cache", helper.run_command, evidence / "objects")
        session.environment = {"SCODE_COMPILER_CACHE_OWNER": "bounded-note-fixture"}
        for phase, text in [("version", "sccache " + cache.VERSION), ("server-start", "Starting sccache server..."),
                            ("initial-stats", stats)]:
            session.tool_command(phase, "-c", "print(" + repr(text) + ")")
        session.current_server = lambda: True
        results = []
        for index in range(15):
            upcoming[0] = [Child(names[child % len(names)]) for child in range(children.get(index, 0))]
            argument = "x" * padding.get(index, 40)
            command = [sys.executable, "-c", "import time; time.sleep(0.3)", argument]
            execute = session.execute if index in {9, 11, 12, 13, 14} else helper.run_command
            results.append(execute(command, evidence, profile / f"{index:03d}.log", {"SCODE_TEST_BACKEND": "mock"}))
        for phase, text in [("final-stats", stats), ("server-stop", "Stopping sccache server...")]:
            session.tool_command(phase, "-c", "print(" + repr(text) + ")")
        session.current_server = lambda: None
        session.close()
    finally:
        if real is None:
            sys.modules.pop("psutil", None)
        else:
            sys.modules["psutil"] = real
        os.environ.clear()
        os.environ.update(ambient)
    (evidence / "records.json").write_text(json.dumps(results), encoding="utf-8")
    return results


BOUNDED_CHECKS = """import json
import shutil
import sys
from pathlib import Path

EVIDENCE = Path(EVIDENCE_PATH)


def run(name, root, output, base, execute):
    if name == "native-workspace":
        output.mkdir(parents=True, exist_ok=True)
        for log in sorted((EVIDENCE / "native-workspace").glob("*.log")):
            shutil.copyfile(log, output / log.name)
        return json.loads((EVIDENCE / "records.json").read_text(encoding="utf-8"))
    code = "from pathlib import Path; import sys; Path(sys.argv[1]).open('a').write(sys.argv[2]+chr(10)); print(sys.argv[2])"
    return [execute([sys.executable, "-c", code, str(root.parent / "events.log"), name], root, output / "000.log")]
"""


def bounded_note_history(fixture, source, environment):
    """Publish two carry generations sharing one origin, as a PR that merges main does.

    `first` runs every profile with the produced workspace records, `second`
    runs only the page profile and carries the workspace from `first`, and a
    clean merge of main carries both: the workspace from `first` and the page
    from `second`, whose own evidence carries from `first` too.
    """
    fixture.mkdir()
    work, origin, home, evidence = fixture / "work", fixture / "origin.git", fixture / "author-home", fixture / "evidence"
    env = {**environment, "HOME": str(home), "USERPROFILE": str(home)}

    def run(*args, cwd=work, success=True, error=None):
        result = subprocess.run(list(map(str, args)), cwd=cwd, capture_output=True, text=True,
                                encoding="utf-8", errors="replace", env=env)
        if (result.returncode == 0) != success:
            raise RuntimeError(f"Unexpected exit {result.returncode}: {args}\n{result.stdout}\n{result.stderr}")
        if error is not None:
            assert error in result.stderr, result.stderr
        return result.stdout.strip()

    records = produced_workspace_records(source, evidence)
    run("git", "init", "--bare", origin, cwd=fixture)
    run("git", "init", "--initial-branch=main", work, cwd=fixture)
    run("git", "config", "user.name", "Bounded note E2E")
    run("git", "config", "user.email", "bounded-note@example.invalid")
    run("git", "remote", "add", "origin", origin)
    helper = work / ".github/scripts/local_acceptance.py"
    helper.parent.mkdir(parents=True)
    shutil.copyfile(source / ".github/scripts/local_acceptance.py", helper)
    profiles = work / ".github/local-acceptance/checks.py"
    profiles.parent.mkdir()
    profiles.write_text(BOUNDED_CHECKS.replace("EVIDENCE_PATH", repr(str(evidence))), encoding="utf-8")
    (profiles.parent / "Dockerfile").write_text("FROM scratch\n")
    (work / ".github/workflows").mkdir()
    for name in ["local-acceptance.yml", "local-acceptance-publish.yml"]:
        (work / ".github/workflows" / name).write_text("# Local enforcement fixture\n")
    (work / "spec").mkdir()
    (work / "spec/local_acceptance.json").write_text(json.dumps({
        "version": 1, "repository": "acceptance/bounded-note-fixture", "checks": {
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
    run("git", "commit", "-m", "Protected bounded-note fixture")
    run("git", "push", "origin", "main")
    run("git", "checkout", "-b", "feature")

    def commit(message, files):
        for path, text in files.items():
            (work / path).write_text(text)
        run("git", "add", ".")
        run("git", "commit", "-m", message)
        return run("git", "rev-parse", "HEAD")

    def check():
        run(sys.executable, helper, "check", "--base", "main", "--policy-ref", "main", "--publish")
        return run("git", "rev-parse", "HEAD")

    first = commit("Change product and page", {"src/core.txt": "pr core\n", "web/page.txt": "pr page\n"})
    check()
    second = commit("Change only the page", {"web/page.txt": "pr page 2\n"})
    check()
    run("git", "checkout", "main")
    commit("Main-only product change", {"src/shared.txt": "main shared\n"})
    run("git", "push", "origin", "main")
    run("git", "checkout", "feature")
    run("git", "merge", "--no-edit", "main")
    merged = check()
    return {"run": run, "commit": commit, "check": check, "helper": helper, "key": key, "home": home,
            "records": records, "first": first, "second": second, "merged": merged,
            "note": published_note(run, merged)}


def published_note(run, head):
    run("git", "fetch", "origin", "+refs/notes/local-acceptance:refs/notes/local-acceptance")
    return run("git", "notes", "--ref=local-acceptance", "show", head).encode()


def verify_bounded_note(directory, source, environment):
    """A realistic note stays well under read_note's 2 MB cap, and its store cannot be forged."""
    fixture = directory / "bounded-note"
    history = bounded_note_history(fixture, source, environment)
    run, helper, key = history["run"], history["helper"], history["key"]
    first, second, merged = history["first"], history["second"], history["merged"]
    records = history["records"]

    # The executor and cache session sign bounded summaries of every child.
    outer = [counts for record in records for counts in record["compiler_priorities"].values()]
    expected = "16384" if sys.platform == "win32" else "10"
    assert sum(count for counts in outer for count in counts.values()) == 3183, outer
    assert all(set(counts) == {expected} for counts in outer), outer
    assert all(len(canonical(record["compiler_priorities"])) < 200 for record in records)
    cached = [record["compiler_cache"] for record in records if "compiler_cache" in record]
    assert len(cached) == 5 and len({entry["session_sha256"] for entry in cached}) == 1, cached
    assert ["session" in entry for entry in cached] == [True, False, False, False, False], cached
    session = cached[0]["session"]
    assert hashlib.sha256(canonical(session)).hexdigest() == cached[0]["session_sha256"]
    assert sum(sum(counts.values()) for counts in session["compiler_priorities"].values()) == 3160, session
    assert [command["phase"] for command in session["tool_commands"]] == [
        "version", "server-start", "initial-stats", "final-stats", "server-stop"], session["tool_commands"]

    # Each unique signed envelope appears once: two references to `first`
    # share one stored copy, and no payload embeds another.
    note = json.loads(history["note"])
    attestation = note["attestations"]["fixture"]
    top = json.loads(base64.b64decode(attestation["payload"]))
    assert top["version"] == 2 and all("envelope" not in group for group in top["carried"]), top["carried"]
    stored = note["envelopes"]
    payloads = {json.loads(base64.b64decode(envelope["payload"]))["head"]: envelope for envelope in stored.values()}
    assert set(payloads) == {first, second}, sorted(payloads)
    for identity, envelope in stored.items():
        assert hashlib.sha256(canonical(envelope)).hexdigest() == identity
    by_head = {group["head"]: group for group in top["carried"]}
    assert {head: group["profiles"] for head, group in by_head.items()} == {
        first: ["native-workspace"], second: ["browser"]}, top["carried"]
    nested = json.loads(base64.b64decode(payloads[second]["payload"]))
    assert [group["envelope_sha256"] for group in nested["carried"]] == [by_head[first]["envelope_sha256"]]
    text = history["note"].decode()
    assert all(text.count(envelope["payload"]) == 1 for envelope in [attestation, *stored.values()])
    unique = sum(len(canonical(envelope)) for envelope in [attestation, *stored.values()])
    assert len(history["note"]) < unique + 1024, (len(history["note"]), unique)
    # Each of the three receipts signs the workspace records it ran or carried,
    # so the note holds three bounded copies: about a tenth of the 2 MB cap.
    assert len(history["note"]) < 300_000, len(history["note"])
    run(sys.executable, helper, "verify", "--head", merged, "--base", "main", "--policy-ref", "main")
    print("BOUNDED_NOTE=" + json.dumps({"note_bytes": len(history["note"]), "unique_envelopes": 1 + len(stored),
                                        "compiler_children": 3183, "cached_commands": len(cached)}), flush=True)

    def write_note(head, value):
        path = fixture / "replaced-note.json"
        path.write_bytes(canonical(value))
        run("git", "notes", "--ref=local-acceptance", "add", "-f", "-F", path, head)

    def refused(head, value, error):
        write_note(head, value)
        run(sys.executable, helper, "verify", "--head", head, "--base", "main", "--policy-ref", "main",
            success=False, error=error)

    # A dangling reference, a changed stored envelope and a re-keyed one fail.
    shared = by_head[first]["envelope_sha256"]
    dangling = {**note, "envelopes": {identity: envelope for identity, envelope in stored.items() if identity != shared}}
    refused(merged, dangling, "missing from its envelope store")
    changed = json.loads(base64.b64decode(stored[shared]["payload"]))
    changed["finished_at"] += 1
    tampered = {**stored[shared], "payload": base64.b64encode(canonical(changed)).decode()}
    refused(merged, {**note, "envelopes": {**stored, shared: tampered}}, "differs from its signed SHA-256")
    rekeyed = {identity: envelope for identity, envelope in stored.items() if identity != shared}
    rekeyed[hashlib.sha256(canonical(tampered)).hexdigest()] = tampered
    refused(merged, {**note, "envelopes": rekeyed}, "missing from its envelope store")

    def sign(payload, label):
        data = fixture / (label + "-receipt.json")
        data.write_bytes(canonical(payload))
        namespace = "sudocode-local-acceptance" + ("-checkpoint" if payload.get("kind") == "completed-profiles" else "")
        run("ssh-keygen", "-Y", "sign", "-f", key, "-n", namespace, data)
        return {"signer": "fixture", "payload": base64.b64encode(data.read_bytes()).decode(),
                "signature": base64.b64encode(data.with_suffix(".json.sig").read_bytes()).decode()}

    # A version 2 receipt cannot embed an envelope, even a correctly signed one.
    embedded = {**top, "carried": [{"head": group["head"], "profiles": group["profiles"],
                                    "envelope": stored[group["envelope_sha256"]]} for group in top["carried"]]}
    refused(merged, {"version": 1, "attestations": {"fixture": sign(embedded, "embedded-v2")}},
            "must reference its origin envelope by SHA-256")

    # Version 1 receipts embedded each carried envelope. Rewrite this history
    # in that form, then verify it and carry from it through the same verifier.
    def legacy(envelope, label):
        payload = json.loads(base64.b64decode(envelope["payload"]))
        if not payload.get("carried"):
            return envelope
        payload["version"] = 1
        payload["carried"] = [{"head": group["head"], "profiles": group["profiles"],
                               "envelope": legacy(stored[group["envelope_sha256"]], label + "-" + group["head"][:12])}
                              for group in payload["carried"]]
        return sign(payload, label)

    old = legacy(attestation, "legacy")
    write_note(merged, {"version": 1, "attestations": {"fixture": old}})
    run(sys.executable, helper, "verify", "--head", merged, "--base", "main", "--policy-ref", "main")
    outputs = sorted((history["home"] / ".nexus/local-acceptance/results" / merged).iterdir())
    output = outputs[-1]
    (output / "receipt.json").write_bytes(base64.b64decode(old["payload"]))
    (output / "envelope.json").write_bytes(canonical(old))
    (output / "envelopes.json").unlink()
    later = history["commit"]("Another page-only commit", {"web/page.txt": "pr page 3\n"})
    history["check"]()
    carried = json.loads(published_note(run, later))
    receipt = json.loads(base64.b64decode(carried["attestations"]["fixture"]["payload"]))
    assert receipt["version"] == 2 and receipt["carried"] == [
        {"head": first, "profiles": ["native-workspace"], "envelope_sha256": shared}], receipt["carried"]
    assert carried["envelopes"] == {shared: stored[shared]}, sorted(carried["envelopes"])
    run(sys.executable, helper, "verify", "--head", later, "--base", "main", "--policy-ref", "main")
    print("PASS: a realistic two-generation note with a shared origin and five commands sharing one compiler "
          "session stays well under 2 MB; dangling, changed, re-keyed and embedded-version-2 references fail; "
          "version 1 receipts still verify and carry through the same verifier, stored once by reference", flush=True)


if __name__ == "__main__":
    from run import SOURCE, FIXTURE_ENV

    with tempfile.TemporaryDirectory(prefix="scode-carryover-acceptance-") as temporary:
        verify_carryover(Path(temporary), SOURCE, FIXTURE_ENV)
