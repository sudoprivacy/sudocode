#!/usr/bin/env python3
"""Run commit-bound local acceptance and verify its SSH-signed Git note.

The signature authenticates an enrolled developer's attestation. It does not
make an untrusted developer machine a remotely attested execution environment.
No candidate code is executed by the GitHub verification job.
"""

import argparse
import ast
import base64
import fnmatch
import hashlib
import json
import os
from pathlib import Path
import platform
import shlex
import shutil
import subprocess
import sys
import tempfile
import time


NOTE_REF = "refs/notes/local-acceptance"
POLICY = "spec/local_acceptance.json"
SIGNERS = "spec/local_acceptance_signers.json"
NAMESPACE = "sudocode-local-acceptance"
ROOT = Path(__file__).resolve().parents[2]
MSVC_ENV = None


def git(*args, root=None, data=None, check=True):
    result = subprocess.run(["git", *args], cwd=root or ROOT, input=data,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if check and result.returncode:
        raise RuntimeError(result.stderr.decode(errors="replace").strip())
    return result


def value(*args, root=None):
    return git(*args, root=root).stdout.decode().strip()


def canonical(obj):
    return json.dumps(obj, sort_keys=True, separators=(",", ":")).encode()


def sha(data):
    return hashlib.sha256(data).hexdigest()


def tracked(ref, path):
    return git("show", f"{ref}:{path}").stdout


def load_policy(ref):
    return json.loads(tracked(ref, POLICY))


def contract_hash(ref):
    paths = [POLICY, ".github/scripts/local_acceptance.py", ".github/workflows/local-acceptance.yml",
             ".github/workflows/local-acceptance-publish.yml"]
    paths += value("ls-tree", "-r", "--name-only", ref, ".github/local-acceptance").splitlines()
    return sha(canonical({p: sha(tracked(ref, p)) for p in sorted(paths)}))


def commands_hash(ref):
    # Scheduling, publication and selection edits do not change a completed
    # command's evidence. Bind actual executors/images, then require the current
    # selected check set to be covered. New commands still require a real run.
    source = tracked(ref, ".github/scripts/local_acceptance.py").decode("utf-8")
    function = next(node for node in ast.parse(source).body
                    if isinstance(node, ast.FunctionDef) and node.name == "run_command")
    executor = "\n".join(source.splitlines()[function.lineno - 1:function.end_lineno])
    paths = [".github/local-acceptance/checks.py", ".github/local-acceptance/Dockerfile"]
    return sha(canonical({"executor": sha(executor.encode()), **{p: sha(tracked(ref, p)) for p in paths}}))


def changed_paths(base, head):
    # --no-renames includes both sides of a rename so moving a file out of a
    # guarded directory cannot make its checks disappear.
    return git("diff", "--name-only", "--no-renames", "-z", base, head).stdout.decode().split("\0")[:-1]


def selected(policy, paths):
    return [name for name, rule in policy["checks"].items()
            if name == "preflight" or any(any(fnmatch.fnmatchcase(p, pattern) for pattern in rule["include"])
                   and not any(fnmatch.fnmatchcase(p, pattern) for pattern in rule.get("exclude", []))
                   for p in paths)]


def clean(head):
    if value("rev-parse", "HEAD") != head:
        raise RuntimeError("HEAD changed during acceptance")
    # Untracked build outputs are ignored by Git. An untracked source file is
    # not a verified input, even if Cargo happened to compile it successfully.
    if value("status", "--porcelain", "--untracked-files=all"):
        raise RuntimeError("Acceptance requires a clean worktree, including untracked source files")


def host():
    system = {"Windows": "windows", "Darwin": "macos", "Linux": "linux"}.get(platform.system())
    if system is None:
        raise RuntimeError("Supported author hosts are Windows, macOS and Linux")
    return {"system": system, "architecture": platform.machine().lower()}


def check_contract_changes(head, base, policy_ref):
    # An existing PR need not merge main merely to acquire newly added hook
    # files. It uses the protected runner. All candidate contract edits,
    # including additional checks, are blocked from authorizing themselves.
    paths = changed_paths(value("merge-base", base, head), head)
    protected = [POLICY, ".github/scripts/local_acceptance.py", ".github/workflows/local-acceptance.yml",
                 ".github/workflows/local-acceptance-publish.yml"]
    if any(path in protected or path.startswith(".github/local-acceptance/") for path in paths):
        if contract_hash(head) != contract_hash(policy_ref):
            changed = sorted(path for path in paths
                             if path in protected or path.startswith(".github/local-acceptance/"))
            raise RuntimeError(
                "Candidate changes the protected acceptance contract. "
                f"Changed protected files: {json.dumps(changed)}. "
                f"Trusted policy: {policy_ref} ({value('rev-parse', policy_ref)}). "
                "Normal acceptance/push blocks every contract change, including added checks; "
                "the candidate cannot authorize its own check definitions. "
                "This runner has no contract-update/bootstrap command. "
                "Repository maintainers must arrange a separately authorized contract update; "
                "this is not a request for the end user to review code or approve this push. "
                "See CONTRIBUTING.md#acceptance-contract-changes."
            )


def run_command(command, cwd, log, env=None):
    """Keep tests Normal while lowering only this command's compiler children."""
    import psutil

    environment = os.environ.copy()
    # Acceptance exercises explicit TrueColor/Ansi256 and NoColor fixtures.
    # A parent's output preference must not suppress the colored fixtures.
    # An explicit command environment may still select NO_COLOR below.
    environment.pop("NO_COLOR", None)
    if os.name == "nt" and not environment.get("LIB") and (
            Path(command[0]).stem.lower() == "cargo" or any("e2e/build-metadata/run.py" in str(arg) for arg in command)):
        global MSVC_ENV
        if MSVC_ENV is None:
            configured = git("config", "--get", "localAcceptance.vsdevcmd", check=False).stdout.decode().strip()
            locator = Path(os.environ.get("ProgramFiles(x86)", "C:/Program Files (x86)")) / "Microsoft Visual Studio/Installer/vswhere.exe"
            if configured:
                batch = Path(configured)
            elif locator.exists():
                installation = subprocess.check_output([str(locator), "-latest", "-products", "*", "-requires",
                                                        "Microsoft.VisualStudio.Component.VC.Tools.x86.x64",
                                                        "-property", "installationPath"], text=True).strip()
                batch = Path(installation) / "Common7/Tools/VsDevCmd.bat"
            else:
                batch = Path("missing-vsdevcmd")
            if not batch.is_file():
                raise RuntimeError("Use Developer PowerShell or set git config localAcceptance.vsdevcmd to your VsDevCmd.bat")
            output = subprocess.check_output(f'cmd.exe /d /s /c ""{batch}" -no_logo -arch=x64 -host_arch=x64 && set"',
                                             encoding="mbcs", errors="replace")
            MSVC_ENV = {key.upper(): value for line in output.splitlines() if "=" in line and not line.startswith("=")
                        for key, value in [line.split("=", 1)]}
        environment.update(MSVC_ENV)
    environment.update({"CARGO_BUILD_JOBS": "1", "CARGO_INCREMENTAL": "0",
                        "CARGO_PROFILE_DEV_DEBUG": "0", "CARGO_PROFILE_TEST_DEBUG": "0", "PYTHONIOENCODING": "utf-8"})
    environment.update(env or {})
    command = [shutil.which(str(command[0])) or str(command[0]), *map(str, command[1:])]
    started = time.monotonic()
    priorities = {}
    log.parent.mkdir(parents=True, exist_ok=True)
    print("+ " + shlex.join(map(str, command)), flush=True)
    with log.open("wb") as stream:
        process = subprocess.Popen(list(map(str, command)), cwd=cwd, env=environment,
                                   stdout=stream, stderr=subprocess.STDOUT)
        try:
            while process.poll() is None:
                try:
                    children = psutil.Process(process.pid).children(recursive=True)
                    for child in children:
                        name = child.name().lower()
                        if name in {"rustc", "rustc.exe", "rust-lld", "rust-lld.exe", "link.exe", "ld", "ld.lld", "cc", "clang", "clang.exe"}:
                            child.nice(psutil.BELOW_NORMAL_PRIORITY_CLASS if os.name == "nt" else 10)
                            priorities[f"{child.pid}:{name}"] = child.nice()
                except (psutil.NoSuchProcess, psutil.AccessDenied):
                    pass
                time.sleep(0.15)
        except BaseException:
            # Interrupting owned validation must not orphan compiler children.
            for child in psutil.Process(process.pid).children(recursive=True)[::-1]:
                try:
                    child.terminate()
                except psutil.NoSuchProcess:
                    pass
            process.terminate()
            process.wait()
            raise
    result = {"command": list(map(str, command)), "exit_code": process.returncode,
              "elapsed_seconds": round(time.monotonic() - started, 3),
              "log_sha256": sha(log.read_bytes()), "compiler_priorities": priorities}
    log.with_suffix(".command.json").write_bytes(canonical(result))
    if process.returncode:
        print(log.read_text(errors="replace")[-10000:], file=sys.stderr)
        raise RuntimeError(f"Local acceptance failed ({process.returncode}); evidence: {log}")
    return result


def read_note(head, ref=NOTE_REF):
    result = git("notes", f"--ref={ref}", "show", head, check=False)
    if result.returncode:
        return {"version": 1, "attestations": {}}
    if len(result.stdout) > 2_000_000:
        raise RuntimeError("Acceptance note exceeds 2 MB")
    obj = json.loads(result.stdout)
    if obj.get("version") != 1 or not isinstance(obj.get("attestations"), dict):
        raise RuntimeError("Invalid acceptance note")
    return obj


def verify_envelope(envelope, head, base, policy_ref):
    payload = base64.b64decode(envelope["payload"], validate=True)
    receipt = json.loads(payload)
    signer_id = envelope["signer"]
    registry = json.loads(tracked(policy_ref, SIGNERS))["signers"]
    signer = registry.get(signer_id)
    if not signer:
        raise RuntimeError(f"Unenrolled acceptance signer: {signer_id}")
    if receipt.get("host", {}).get("system") != signer["platform"]:
        raise RuntimeError("Signer used the wrong native platform")
    with tempfile.TemporaryDirectory(prefix="scode-verify-") as folder:
        directory = Path(folder)
        (directory / "allowed").write_text(f"{signer_id} {signer['public_key']}\n", encoding="utf-8")
        (directory / "proof.sig").write_bytes(base64.b64decode(envelope["signature"], validate=True))
        result = subprocess.run(["ssh-keygen", "-Y", "verify", "-f", str(directory / "allowed"),
                                 "-I", signer_id, "-n", NAMESPACE, "-s", str(directory / "proof.sig")],
                                input=payload, capture_output=True)
        if result.returncode:
            raise RuntimeError("Invalid SSH acceptance signature")
    if receipt.get("version") != 1 or receipt.get("repository") != load_policy(policy_ref)["repository"]:
        raise RuntimeError("Wrong acceptance schema or repository")
    if receipt.get("head") != head or receipt.get("tree") != value("rev-parse", f"{head}^{{tree}}"):
        raise RuntimeError("Acceptance is for another commit or tree")
    if receipt.get("base") != value("merge-base", base, head):
        raise RuntimeError("Acceptance used a different PR base")
    compatible = receipt.get("commands_sha256") == commands_hash(policy_ref)
    if not compatible and "commands_sha256" not in receipt:
        # Bootstrap receipts used the whole protected contract. Accept them
        # only when that signed hash matches their actual protected base and
        # the commands are byte-identical to today's executor/profile/image.
        try:
            compatible = (receipt.get("contract_sha256") == contract_hash(receipt["base"])
                          and commands_hash(receipt["base"]) == commands_hash(policy_ref))
        except RuntimeError:
            compatible = False
    if not compatible:
        raise RuntimeError("Acceptance commands changed; rerun local acceptance")
    check_contract_changes(head, base, policy_ref)
    required = selected(load_policy(policy_ref), changed_paths(receipt["base"], head))
    checks = receipt.get("checks", {})
    if not set(required).issubset(checks):
        raise RuntimeError(f"Acceptance check set differs from policy: need {required}")
    for name, commands in checks.items():
        if not isinstance(commands, list) or not commands:
            raise RuntimeError(f"Missing actual commands for {name}")
        for command in commands:
            if command.get("exit_code") != 0 or not command.get("command") or len(command.get("log_sha256", "")) != 64:
                raise RuntimeError(f"Incomplete/failed command in {name}")
    print(f"Verified {head[:12]} on {signer['platform']} ({signer_id}): {', '.join(required)}", flush=True)
    return receipt


def verify(head, base, policy_ref, note_ref=NOTE_REF):
    errors = []
    for envelope in read_note(head, note_ref)["attestations"].values():
        try:
            return verify_envelope(envelope, head, base, policy_ref)
        except (RuntimeError, ValueError, KeyError) as error:
            errors.append(str(error))
    raise RuntimeError("No current valid local acceptance: " + ("; ".join(errors) or "signed note missing"))


def publish(head, envelope, remote):
    # Use a private temporary notes ref. Fetch+lease retries preserve evidence
    # another author publishes concurrently; never overwrite their notes tree.
    temporary = f"refs/notes/scode-publish-{os.getpid()}"
    try:
        for _attempt in range(5):
            existing = value("ls-remote", remote, NOTE_REF).split()
            previous = existing[0] if existing else ""
            if previous:
                git("fetch", "--no-tags", remote, f"+{NOTE_REF}:{temporary}")
            else:
                git("update-ref", "-d", temporary)
            notes = read_note(head, temporary)
            notes["attestations"][envelope["signer"]] = envelope
            git("notes", f"--ref={temporary}", "add", "-f", "-F", "-", head, data=canonical(notes))
            result = git("push", f"--force-with-lease={NOTE_REF}:{previous}", remote,
                         f"{temporary}:{NOTE_REF}", check=False)
            if result.returncode == 0:
                return
            if "stale info" not in result.stderr.decode(errors="replace"):
                raise RuntimeError(result.stderr.decode(errors="replace"))
        raise RuntimeError("Concurrent acceptance publishers: retry the push")
    finally:
        git("update-ref", "-d", temporary)


def check(args):
    import importlib.util

    head = value("rev-parse", args.head or "HEAD")
    clean(head)
    policy_ref = args.policy_ref
    # A candidate cannot replace its required command definitions unnoticed.
    check_contract_changes(head, args.base, policy_ref)
    base = value("merge-base", args.base, head)
    needed = selected(load_policy(policy_ref), changed_paths(base, head))
    identity = value("config", "localAcceptance.signer")
    key = Path(value("config", "localAcceptance.key")).expanduser()
    registry = json.loads(tracked(policy_ref, SIGNERS))["signers"]
    if identity not in registry or registry[identity]["platform"] != host()["system"]:
        raise RuntimeError("Enroll this host's public signing key on main before running acceptance")
    output = Path(args.output or (Path.home() / ".nexus/local-acceptance/results" / head / str(time.time_ns()))).resolve()
    if output == ROOT or ROOT in output.parents:
        raise RuntimeError("Keep acceptance outputs outside the source checkout")
    output.mkdir(parents=True, exist_ok=False)
    profiles = output / "protected_profiles.py"
    profiles.write_bytes(tracked(policy_ref, ".github/local-acceptance/checks.py"))
    spec = importlib.util.spec_from_file_location("acceptance_checks", profiles)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    results = {}
    for name in needed:
        print(f"Running {name} on {host()['system']}; logs: {output}", flush=True)
        results[name] = module.run(name, ROOT, output / name, base, run_command)
    clean(head)
    receipt = {"version": 1, "repository": load_policy(policy_ref)["repository"],
               "head": head, "tree": value("rev-parse", f"{head}^{{tree}}"), "base": base,
               "contract_sha256": contract_hash(policy_ref), "host": host(),
               "commands_sha256": commands_hash(policy_ref),
               "finished_at": int(time.time()), "checks": results}
    payload = output / "receipt.json"
    payload.write_bytes(canonical(receipt))
    signature = payload.with_suffix(".json.sig")
    if signature.exists():
        signature.unlink()
    subprocess.run(["ssh-keygen", "-Y", "sign", "-f", str(key), "-n", NAMESPACE, str(payload)], check=True)
    envelope = {"signer": identity, "payload": base64.b64encode(payload.read_bytes()).decode(),
                "signature": base64.b64encode(signature.read_bytes()).decode()}
    verify_envelope(envelope, head, args.base, policy_ref)
    (output / "envelope.json").write_bytes(canonical(envelope))
    if args.publish:
        publish(head, envelope, args.remote)
    print(f"Local acceptance complete: {payload}")


def install(args):
    common = Path(value("rev-parse", "--git-common-dir")).resolve()
    existing = git("config", "--get", "core.hooksPath", check=False).stdout.decode().strip()
    managed = common / "local-acceptance-hooks"
    if existing and Path(existing).resolve() != managed:
        raise RuntimeError(f"Existing core.hooksPath needs manual composition: {existing}")
    if not existing and (common / "hooks/pre-push").exists():
        raise RuntimeError("Existing pre-push hook needs manual composition; it was preserved")
    managed.mkdir(parents=True, exist_ok=True)
    # Outside the worktree so an older branch cannot silently remove the hook.
    shutil.copyfile(__file__, managed / "local_acceptance.py")
    interpreter = shlex.quote(sys.executable.replace("\\", "/"))
    helper = shlex.quote(str(managed / "local_acceptance.py").replace("\\", "/"))
    hook = managed / "pre-push"
    hook.write_text(f'#!/bin/sh\nexec {interpreter} {helper} pre-push "$@"\n', encoding="utf-8", newline="\n")
    hook.chmod(0o755)
    git("config", "--local", "core.hooksPath", str(managed))
    if args.signer:
        git("config", "--local", "localAcceptance.signer", args.signer)
    if args.key:
        git("config", "--local", "localAcceptance.key", str(Path(args.key).expanduser().resolve()))
    print(f"Installed automatic pre-push acceptance for all worktrees: {managed}")


def pre_push(args):
    global ROOT
    ROOT = Path(value("rev-parse", "--show-toplevel", root=Path.cwd()))
    updates = [line.split() for line in sys.stdin.read().splitlines() if line.strip()]
    heads = [row[1] for row in updates if row[2].startswith("refs/heads/") and set(row[1]) != {"0"}]
    if not heads:
        return
    git("fetch", "--no-tags", "origin", "+refs/heads/main:refs/remotes/origin/main")
    current_runner = tracked("origin/main", ".github/scripts/local_acceptance.py")
    if sha(Path(__file__).read_bytes()) != sha(current_runner):
        # Upgrade from the protected default branch, including old worktrees.
        # Do not import or execute the candidate's replacement runner.
        Path(__file__).write_bytes(current_runner)
        result = subprocess.run([sys.executable, __file__, "pre-push", args.remote, args.url],
                                input=("\n".join(" ".join(row) for row in updates) + "\n").encode())
        if result.returncode:
            raise RuntimeError("Updated local acceptance hook rejected the push")
        return
    for head in set(heads):
        if head != value("rev-parse", "HEAD"):
            raise RuntimeError("Push one checked-out branch at a time so acceptance tests its exact sources")
        fetched = git("fetch", "--no-tags", args.remote, f"+{NOTE_REF}:{NOTE_REF}", check=False)
        if fetched.returncode and "couldn't find remote ref" not in fetched.stderr.decode(errors="replace"):
            raise RuntimeError(fetched.stderr.decode(errors="replace"))
        try:
            verify(head, "origin/main", "origin/main")
        except RuntimeError:
            # Use the installed protected runner, never a candidate's replacement.
            check(argparse.Namespace(head=head, base="origin/main", policy_ref="origin/main",
                                     output=None, publish=True, remote=args.remote))


def main():
    global ROOT
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="operation", required=True)
    for name in ["check", "verify"]:
        command = sub.add_parser(name)
        command.add_argument("--head")
        command.add_argument("--base", default="origin/main")
        command.add_argument("--policy-ref", default="origin/main")
        if name == "check":
            command.add_argument("--output")
            command.add_argument("--publish", action="store_true")
            command.add_argument("--remote", default="origin")
    command = sub.add_parser("install")
    command.add_argument("--signer")
    command.add_argument("--key")
    command = sub.add_parser("pre-push")
    command.add_argument("remote")
    command.add_argument("url")
    args = parser.parse_args()
    try:
        # Installed runners live outside a worktree. Their caller's checkout,
        # not the installation directory, supplies the exact tested sources.
        ROOT = Path(value("rev-parse", "--show-toplevel", root=Path.cwd()))
        if args.operation == "verify":
            verify(value("rev-parse", args.head or "HEAD"), args.base, args.policy_ref)
        elif args.operation == "check":
            check(args)
        elif args.operation == "install":
            install(args)
        else:
            pre_push(args)
    except (RuntimeError, subprocess.CalledProcessError, OSError, ValueError, KeyError) as error:
        print(f"Local acceptance blocked: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
