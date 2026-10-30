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
                run(sys.executable, trusted, "verify", "--head", head, "--policy-ref", changed,
                    success=False, error="Acceptance commands changed")

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
        run("git", "checkout", original_branch)
        print("PASS: real hook pushes preserve modern/legacy SSH notes without repeating unchanged execution; "
              "selected/shared/executor/image changes, missing profiles and malformed fingerprints fail")
    finally:
        install_note(original_note)
        run("git", "fetch", str(work), f"+{original_main}:refs/heads/main", cwd=origin)
        run("git", "fetch", "origin", "+refs/heads/main:refs/remotes/origin/main")
