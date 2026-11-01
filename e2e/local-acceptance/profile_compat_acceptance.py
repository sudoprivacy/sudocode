"""Exercise selected execution compatibility through real Git hooks and SSH notes."""

import base64
import json
from pathlib import Path
import shutil
import sys


def verify_profile_compatibility(directory, work, command):
    def run(*args, cwd=work, **kwargs):
        return command(*args, cwd=cwd, **kwargs)

    fixture = directory / "profile-compat"
    fixture.mkdir()
    origin = Path(run("git", "remote", "get-url", "origin"))
    original_branch = run("git", "branch", "--show-current")
    original_main = run("git", "rev-parse", "origin/main")
    head = run("git", "rev-parse", "HEAD")
    original_note = run("git", "notes", "--ref=local-acceptance", "show", head)
    helper = work / ".github/scripts/local_acceptance.py"
    trusted = fixture / "trusted.py"
    trusted.write_text(helper.read_text())
    key = run("git", "config", "localAcceptance.key")
    checks = work / ".github/local-acceptance/checks.py"
    policy = work / "spec/local_acceptance.json"

    def install_note(text):
        note_file = fixture / "note.json"
        note_file.write_text(text)
        run("git", "notes", "--ref=local-acceptance", "add", "-f", "-F", note_file, head)
        # This local bare remote is an enforcement fixture, not a production
        # protection bypass. The actual hook fetches these SSH-signed notes.
        run("git", "fetch", str(work), "+refs/notes/local-acceptance:refs/notes/local-acceptance", cwd=origin)

    def signed_note(payload):
        data = fixture / "payload.json"
        data.write_text(json.dumps(payload))
        signature = data.with_suffix(".json.sig")
        if signature.exists():
            signature.unlink()
        run("ssh-keygen", "-Y", "sign", "-f", key, "-n", "sudocode-local-acceptance", data)
        note = json.loads(original_note)
        envelope = note["attestations"]["fixture"]
        envelope.update(payload=base64.b64encode(data.read_bytes()).decode(),
                        signature=base64.b64encode(signature.read_bytes()).decode())
        return json.dumps(note)

    original_payload = json.loads(base64.b64decode(json.loads(original_note)["attestations"]["fixture"]["payload"]))
    v1_payload = {key: value for key, value in original_payload.items()
                  if key not in {"reachable_profile_commands_sha256", "commands_ref"}}
    legacy_payload = {key: value for key, value in v1_payload.items() if key != "profile_commands_sha256"}
    variants = {"modern": original_note, "v1": signed_note(v1_payload), "legacy": signed_note(legacy_payload)}
    try:
        run("git", "checkout", "-b", "compat-future", "origin/main")
        previous_source = checks.read_text()
        future_source = previous_source.replace(
            '\n    return [execute(',
            '\n    if name == "native-future":\n'
            '        raise RuntimeError("UNSELECTED_PROFILE_EXECUTED")\n'
            '    return [execute(',
        )
        future_source = future_source.replace("unused browser helper v1", "unused browser helper v2")
        assert future_source != previous_source
        checks.write_text(future_source)
        rules = json.loads(policy.read_text())
        rules["checks"]["native-future"] = {"include": ["future/**"]}
        policy.write_text(json.dumps(rules))
        run("git", "add", ".")
        run("git", "commit", "-m", "Add an unselected future native profile")
        future = run("git", "rev-parse", "HEAD")
        run("git", "fetch", str(work), "compat-future:refs/heads/main", cwd=origin)
        run("git", "checkout", original_branch)
        run("git", "fetch", "origin", "main")
        for label, note in variants.items():
            install_note(note)
            output = run("git", "push", "origin", f"HEAD:refs/heads/compat-{label}")
            assert f"Verified {head[:12]}" in output, output
            assert "Running real-process" not in output, "Unchanged real execution was unnecessarily repeated"
            assert run("git", "ls-remote", "origin", f"refs/heads/compat-{label}").split()[0] == head
            assert run("git", "notes", "--ref=local-acceptance", "show", head) == note
            interpreters = {str(Path(sys.executable).resolve())}
            for name in ["python3.12", "python3.14"]:
                executable = shutil.which(name)
                if executable:
                    interpreters.add(str(Path(executable).resolve()))
            for interpreter in sorted(interpreters):
                run(interpreter, trusted, "verify", "--head", head, "--policy-ref", future)

        # Selected commands, common state, process execution and images must
        # reject the genuinely signed prior execution, in both receipt formats.
        helper_source = helper.read_text()
        image = work / ".github/local-acceptance/Dockerfile"
        image_source = image.read_text()
        mutations = [
            ("selected", checks, future_source.replace("Path('input.txt')", "Path('different-input.txt')")),
            ("common", checks, "SHARED_PROFILE_REVISION = 2\n" + future_source),
            ("nested-helper", checks, future_source.replace("return sys.executable", "return sys.executable + '.changed'")),
            ("executor", helper, helper_source.replace('    import psutil\n',
                '    import psutil\n    print("CHANGED_EXECUTOR")\n', 1)),
            ("image", image, image_source + "\nENV SCODE_CHANGED_IMAGE=1\n"),
            ("reflection", checks, future_source + '\ndef reflect_helpers():\n    return globals()\n'),
        ]
        for label, path, content in mutations:
            run("git", "checkout", "-b", "compat-changed-" + label, future)
            assert path.read_text() != content, label
            path.write_text(content)
            run("git", "add", ".")
            run("git", "commit", "-m", "Change " + label + " execution")
            changed = run("git", "rev-parse", "HEAD")
            for note in variants.values():
                install_note(note)
                diagnostic = run(sys.executable, trusted, "verify", "--head", head, "--policy-ref", changed,
                                 success=False, error="Acceptance commands changed", stderr=True)
                records = [json.loads(line.removeprefix("LOCAL_ACCEPTANCE_RECOVERY="))
                           for line in diagnostic.splitlines() if line.startswith("LOCAL_ACCEPTANCE_RECOVERY=")]
                assert len(records) == 1, diagnostic
                recovery = records[0]
                assert recovery["version"] == 1 and recovery["run_on"] == "original author native host", recovery
                assert recovery["ci_proof_host_is_not_the_author_test_host"] is True, recovery
                assert recovery["args"] == ["check", "--head", head, "--base", "origin/main",
                                            "--policy-ref", changed, "--publish", "--remote", "origin"], recovery

        # A newly required profile still needs its own real execution.
        run("git", "checkout", "-b", "compat-required", future)
        rules["checks"]["native-future"] = {"include": ["**"]}
        policy.write_text(json.dumps(rules))
        run("git", "add", ".")
        run("git", "commit", "-m", "Require the future profile")
        required = run("git", "rev-parse", "HEAD")
        for note in variants.values():
            install_note(note)
            run(sys.executable, trusted, "verify", "--head", head, "--policy-ref", required,
                success=False, error="Acceptance commands changed")

        # An enrolled key cannot attach malformed or incorrect new profile
        # fingerprints to otherwise successful command evidence.
        for hashes in [None, {}, {"real-process": "0" * 64}]:
            payload = {**original_payload, "profile_commands_sha256": hashes}
            install_note(signed_note(payload))
            run(sys.executable, trusted, "verify", "--head", head, "--policy-ref", future,
                success=False, error="Acceptance commands changed")
        for hashes in [None, {}, {"real-process": "0" * 64}]:
            payload = {**original_payload, "reachable_profile_commands_sha256": hashes}
            install_note(signed_note(payload))
            run(sys.executable, trusted, "verify", "--head", head, "--policy-ref", future,
                success=False, error="Acceptance commands changed")
        for source in [None, "HEAD", "0" * 40]:
            install_note(signed_note({**original_payload, "commands_ref": source}))
            run(sys.executable, trusted, "verify", "--head", head, "--policy-ref", future,
                success=False, error="Acceptance commands changed")
        install_note(signed_note({**legacy_payload, "commands_sha256": "0" * 64}))
        run(sys.executable, trusted, "verify", "--head", head, "--policy-ref", future,
            success=False, error="Acceptance commands changed")

        # A protected migration may explicitly recognize two equivalent recipes.
        # The approval binds both hashes, so later command changes cannot inherit it.
        run("git", "checkout", "-b", "compat-equivalent", future)
        equivalent_source = future_source.replace("Path('input.txt').read_text()",
            "Path('input.txt').read_text(encoding='utf-8')")
        assert equivalent_source != future_source
        checks.write_text(equivalent_source)
        run("git", "add", ".")
        run("git", "commit", "-m", "Specify UTF8 for the same real assertion")
        executable_hash = json.loads(run(sys.executable, "-c",
            "import json,runpy; h=runpy.run_path('.github/scripts/local_acceptance.py'); "
            "print(json.dumps(h['reachable_profile_commands_hashes']('HEAD',['real-process'])))"))["real-process"]
        equivalence = json.loads(policy.read_text())
        equivalence["equivalent_profile_commands"] = {"real-process": {executable_hash: [
            original_payload["reachable_profile_commands_sha256"]["real-process"]]}}
        policy.write_text(json.dumps(equivalence))
        run("git", "add", ".")
        run("git", "commit", "-m", "Approve the exact equivalent execution hashes")
        approved = run("git", "rev-parse", "HEAD")
        for note in variants.values():
            install_note(note)
            output = run(sys.executable, trusted, "verify", "--head", head, "--policy-ref", approved)
            assert "trusted policy approves this previous execution recipe as equivalent" in output
            assert run("git", "notes", "--ref=local-acceptance", "show", head) == note

        # A grouping-only migration can retain the original target selection
        # only for committed sources without group files or declarations.
        run("git", "checkout", "-b", "compat-conditional", approved)
        conditional = json.loads(json.dumps(equivalence))
        conditional.pop("equivalent_profile_commands")
        conditional["equivalent_profile_commands_without_test_groups"] = {
            "real-process": {executable_hash: [
                original_payload["reachable_profile_commands_sha256"]["real-process"]]}}
        policy.write_text(json.dumps(conditional))
        run("git", "add", ".")
        run("git", "commit", "-m", "Approve original selection only for sources without groups")
        conditional_policy = run("git", "rev-parse", "HEAD")
        for note in variants.values():
            install_note(note)
            output = run(sys.executable, trusted, "verify", "--head", head, "--policy-ref", conditional_policy)
            assert "without test groups" in output, output
            assert "Running real-process" not in output
            assert run("git", "notes", "--ref=local-acceptance", "show", head) == note
        for label, files in [
            ("group-file", {"rust/crates/example/tests/acceptance_groups/known.rs": "// committed group marker\n"}),
            ("escaped-declaration", {"rust/Cargo.toml":
                '[package]\nname="conditional-case"\nversion="0.0.0"\n[[test]]\nname="known"\n'
                'path="tests/acceptance_\\u0067roups/known.rs"\ntest=false\n'}),
            ("unknown-manifest", {"rust/Cargo.toml": "[invalid\n"}),
            ("external-workspace", {"rust/Cargo.toml": '[workspace]\nmembers=["../external"]\n'}),
        ]:
            run("git", "checkout", "-b", "compat-conditional-" + label, head)
            for relative, content in files.items():
                destination = work / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                destination.write_text(content)
            run("git", "add", ".")
            run("git", "commit", "-m", "Commit conditional selection boundary " + label)
            candidate = run("git", "rev-parse", "HEAD")
            output = run(sys.executable, trusted, "check", "--head", candidate,
                         "--base", original_main, "--policy-ref", original_main,
                         "--output", fixture / ("conditional-" + label), "--publish")
            assert "Running real-process" in output, "Each fixture head needs an actual new native-process receipt"
            run("git", "fetch", "origin", "+refs/notes/local-acceptance:refs/notes/local-acceptance")
            run(sys.executable, trusted, "verify", "--head", candidate,
                "--base", original_main, "--policy-ref", conditional_policy,
                success=False, error="Acceptance commands changed")
        run("git", "checkout", "compat-equivalent")
        print("PASS: modern/legacy real SSH notes retain ungrouped source execution; committed groups, escaped TOML declarations, malformed metadata and external workspaces require fresh validation")

        checks.write_text(equivalent_source.replace("raise SystemExit(text != 'pass')",
                                                   "raise SystemExit(text != 'different')"))
        run("git", "add", ".")
        run("git", "commit", "-m", "Change the actual assertion after migration")
        changed = run("git", "rev-parse", "HEAD")
        for note in variants.values():
            install_note(note)
            run(sys.executable, trusted, "verify", "--head", head, "--policy-ref", changed,
                success=False, error="Acceptance commands changed")
        run("git", "checkout", "-b", "compat-equivalent-required", approved)
        equivalence["checks"]["native-future"] = {"include": ["**"]}
        policy.write_text(json.dumps(equivalence))
        run("git", "add", ".")
        run("git", "commit", "-m", "Require a new check outside the equivalent pair")
        changed = run("git", "rev-parse", "HEAD")
        for note in variants.values():
            install_note(note)
            run(sys.executable, trusted, "verify", "--head", head, "--policy-ref", changed,
                success=False, error="Acceptance commands changed")
        # A diagnostic captured before another protected-main update must still
        # recover against current main when the author follows it later.
        run("git", "checkout", "-b", "compat-recovery-main", future)
        checks.write_text("SHARED_PROFILE_REVISION = 2\n" + future_source)
        run("git", "add", ".")
        run("git", "commit", "-m", "Advance the protected execution recipe")
        diagnostic_policy = run("git", "rev-parse", "HEAD")
        run("git", "fetch", str(work), "compat-recovery-main:refs/heads/main", cwd=origin)
        run("git", "checkout", original_branch)
        run("git", "fetch", "origin", "+refs/heads/main:refs/remotes/origin/main")
        install_note(original_note)
        diagnostic = run(sys.executable, trusted, "verify", "--head", head,
                         success=False, error="Acceptance commands changed", stderr=True)
        recovery = next(json.loads(line.removeprefix("LOCAL_ACCEPTANCE_RECOVERY="))
                        for line in diagnostic.splitlines() if line.startswith("LOCAL_ACCEPTANCE_RECOVERY="))
        run("git", "checkout", "compat-recovery-main")
        checks.write_text("SHARED_PROFILE_REVISION = 3\n" + future_source)
        current_rules = json.loads(policy.read_text())
        current_rules["checks"]["real-process-recovery"] = {"include": ["**"]}
        policy.write_text(json.dumps(current_rules))
        run("git", "add", ".")
        run("git", "commit", "-m", "Require another real process after the diagnostic")
        current_policy = run("git", "rev-parse", "HEAD")
        run("git", "fetch", str(work), "compat-recovery-main:refs/heads/main", cwd=origin)
        run("git", "checkout", original_branch)
        # Deliberately keep the local tracking ref stale until the advertised
        # fetch step. The old instruction pins diagnostic_policy even on a hit.
        assert run("git", "rev-parse", "origin/main") == diagnostic_policy
        for prepare in recovery.get("prepare", []):
            run(*prepare)
        fresh_output = fixture / "fresh-recovery"
        run(sys.executable, trusted, *recovery["args"], "--output", fresh_output)
        fresh = json.loads((fresh_output / "receipt.json").read_text())
        assert fresh["commands_ref"] == current_policy, "Recovery instructions pinned an obsolete protected policy"
        assert set(fresh["checks"]) == {"real-process", "real-process-recovery"}
        assert (fresh_output / "real-process-recovery/actual-process.log").read_text().strip() == "pass"
        # Publication updates the remote notes tree through a private temporary
        # ref. Fetch exactly what CI consumes instead of reading the old note.
        run("git", "fetch", "origin", "+refs/notes/local-acceptance:refs/notes/local-acceptance")
        published = json.loads(run("git", "notes", "--ref=local-acceptance", "show", head))
        published_payload = json.loads(base64.b64decode(published["attestations"]["fixture"]["payload"]))
        assert published_payload == fresh, "CI must receive the actual newly completed signed receipt"
        run(sys.executable, trusted, "verify", "--head", head)
        assert run("git", "rev-parse", "HEAD") == head
        recovery_result = {
            "diagnostic_policy": diagnostic_policy, "current_policy": current_policy,
            "actual_receipt_policy": fresh["commands_ref"], "head": head,
            "actual_completed_checks": sorted(fresh["checks"]), "recovery": recovery,
        }
        (fixture / "fresh-recovery-result.json").write_text(json.dumps(recovery_result, indent=2) + "\n")
        print("FRESH_RECOVERY_RESULT=" + json.dumps(recovery_result))
        print("PASS: captured author recovery fetches current protected main, runs its newly required real process, signs/publishes and verifies the unchanged candidate head")
        run("git", "checkout", original_branch)
        print("PASS: real hook pushes preserve modern/legacy SSH notes without repeating unchanged execution; "
              "selected/shared/executor/image changes, missing profiles and malformed fingerprints fail; "
              "explicit equivalent recipe pairs preserve signed notes but reject changed assertions/new checks")
    finally:
        install_note(original_note)
        run("git", "fetch", str(work), f"+{original_main}:refs/heads/main", cwd=origin)
        run("git", "fetch", "origin", "+refs/heads/main:refs/remotes/origin/main")
