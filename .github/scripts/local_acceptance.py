#!/usr/bin/env python3
"""Run commit-bound local acceptance and verify its SSH-signed Git note.

The signature authenticates an enrolled developer's attestation. It does not
make an untrusted developer machine a remotely attested execution environment.
No candidate code is executed by the GitHub verification job.
"""

import argparse
import ast
import base64
import copy
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
import urllib.error
import urllib.parse


NOTE_REF = "refs/notes/local-acceptance"
POLICY = "spec/local_acceptance.json"
SIGNERS = "spec/local_acceptance_signers.json"
NAMESPACE = "sudocode-local-acceptance"
AUTHORIZATION_CONTEXT = "Acceptance contract authorization"
CONTRACT_UPDATE_COMMAND = ('python "$(git rev-parse --path-format=absolute --git-common-dir)/'
                           'local-acceptance-hooks/local_acceptance.py" contract-update --remote origin')
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
    paths = [POLICY, SIGNERS, ".github/scripts/local_acceptance.py", ".github/workflows/local-acceptance.yml",
             ".github/workflows/local-acceptance-publish.yml"]
    paths += value("ls-tree", "-r", "--name-only", ref, ".github/local-acceptance").splitlines()
    return sha(canonical({p: sha(tracked(ref, p)) for p in sorted(paths)}))


def executor_source(ref):
    source = tracked(ref, ".github/scripts/local_acceptance.py").decode("utf-8")
    function = next(node for node in ast.parse(source).body
                    if isinstance(node, ast.FunctionDef) and node.name == "run_command")
    return "\n".join(source.splitlines()[function.lineno - 1:function.end_lineno])


def commands_hash(ref):
    # Scheduling, publication and selection edits do not change a completed
    # command's evidence. Bind actual executors/images, then require the current
    # selected check set to be covered. New commands still require a real run.
    paths = [".github/local-acceptance/checks.py", ".github/local-acceptance/Dockerfile"]
    return sha(canonical({"executor": sha(executor_source(ref).encode()),
                          **{p: sha(tracked(ref, p)) for p in paths}}))


def profile_ast_value(value):
    """Serialize semantic AST fields independently of Python's dump formatting."""
    if isinstance(value, ast.AST):
        return {"node": type(value).__name__, "fields": {
            name: profile_ast_value(field) for name, field in ast.iter_fields(value)
            if field is not None and not (isinstance(field, list) and not field)
        }}
    if isinstance(value, list):
        return [profile_ast_value(item) for item in value]
    if isinstance(value, bytes):
        return {"bytes": value.hex()}
    if isinstance(value, complex):
        return {"complex": [value.real, value.imag]}
    if value is Ellipsis:
        return {"ellipsis": True}
    if value is None or isinstance(value, (str, int, float, bool)):
        return value
    raise RuntimeError(f"Unsupported acceptance AST value: {type(value).__name__}")


def profile_commands_hashes(ref, names):
    """Bind each literal dispatch branch and all common code without importing it."""
    module = ast.parse(tracked(ref, ".github/local-acceptance/checks.py"))
    executor = sha(executor_source(ref).encode())
    image = sha(tracked(ref, ".github/local-acceptance/Dockerfile"))
    result = {}
    for name in names:
        projected = copy.deepcopy(module)
        dispatcher = next(node for node in projected.body
                          if isinstance(node, ast.FunctionDef) and node.name == "run")
        parameter = dispatcher.args.args[0].arg

        def project(node):
            test = node.test if isinstance(node, ast.If) else None
            if (not isinstance(test, ast.Compare) or not isinstance(test.left, ast.Name)
                    or test.left.id != parameter or len(test.ops) != 1 or not isinstance(test.ops[0], ast.Eq)
                    or len(test.comparators) != 1 or not isinstance(test.comparators[0], ast.Constant)
                    or not isinstance(test.comparators[0].value, str)):
                return [node]
            tail = project(node.orelse[0]) if len(node.orelse) == 1 else node.orelse
            if test.comparators[0].value != name:
                return tail
            node.orelse = tail
            return [node]

        # If the dispatcher rewrites its profile parameter, its branches may
        # cease to be disjoint. Keep the whole module in that conservative case.
        reassigned = any(isinstance(node, ast.Name) and node.id == parameter and isinstance(node.ctx, ast.Store)
                         for node in ast.walk(dispatcher))
        if not reassigned:
            dispatcher.body = [child for node in dispatcher.body for child in project(node)]
        result[name] = sha(canonical({"version": 1, "executor": executor, "image": image,
                                     "profile": profile_ast_value(projected)}))
    return result


def compatible_profile_commands(receipt, policy_ref, required):
    recorded = receipt.get("profile_commands_sha256")
    try:
        if "profile_commands_sha256" not in receipt:
            # Legacy receipts can recover their attested code only when the
            # immutable signed source contains that exact executor and image.
            previous = None
            for candidate in [receipt["head"], receipt["base"]]:
                try:
                    if receipt.get("commands_sha256") == commands_hash(candidate):
                        previous = candidate
                        break
                except (RuntimeError, subprocess.CalledProcessError, StopIteration):
                    continue
            if previous is None:
                return False
            recorded = profile_commands_hashes(previous, required)
        current = profile_commands_hashes(policy_ref, required)
        return isinstance(recorded, dict) and all(recorded.get(name) == current[name] for name in required)
    except (RuntimeError, subprocess.CalledProcessError, SyntaxError, StopIteration, IndexError, KeyError):
        return False


def changed_paths(base, head):
    # --no-renames includes both sides of a rename so moving a file out of a
    # guarded directory cannot make its checks disappear.
    return git("diff", "--name-only", "--no-renames", "-z", base, head).stdout.decode().split("\0")[:-1]


def selected(policy, paths):
    return [name for name, rule in policy["checks"].items()
            if name == "preflight" or any(any(fnmatch.fnmatchcase(p, pattern) for pattern in rule["include"])
                   and not any(fnmatch.fnmatchcase(p, pattern) for pattern in rule.get("exclude", []))
                   for p in paths)]


NATIVE_TARGETS = {
    "linux": {"system": "linux", "runner": "ubuntu-latest", "label": "Linux"},
    "linux-x64": {"system": "linux", "architecture": "x64", "runner": "ubuntu-latest", "label": "Linux x64"},
    "linux-arm64": {"system": "linux", "architecture": "arm64", "runner": "ubuntu-24.04-arm", "label": "Linux ARM64"},
    "macos": {"system": "macos", "runner": "macos-latest", "label": "macOS"},
    "windows": {"system": "windows", "runner": "windows-latest", "label": "Windows"},
}
DEFAULT_NATIVE_TARGETS = ["linux", "macos", "windows"]


def acceptance_plan(policy, names, author):
    """One plan drives local receipts, missing-host CI and agent-visible reports."""
    architecture = {"x86_64": "x64", "amd64": "x64", "aarch64": "arm64"}.get(
        author["architecture"].lower(), author["architecture"].lower())
    if author["system"] not in {"linux", "macos", "windows"}:
        raise RuntimeError("Unknown author platform; no CI coverage may be inferred")
    jobs = {}
    local_targets = []
    for name in names:
        if not name.startswith("native-"):
            continue
        targets = policy["checks"][name].get("targets", DEFAULT_NATIVE_TARGETS)
        if not isinstance(targets, list) or not targets or any(not isinstance(target, str) for target in targets):
            raise RuntimeError(f"Invalid acceptance targets for {name}")
        if len(targets) != len(set(targets)):
            raise RuntimeError(f"Duplicate acceptance targets for {name}")
        covered = []
        for identifier in targets:
            if identifier not in NATIVE_TARGETS:
                raise RuntimeError(f"Unknown acceptance target: {identifier}")
            target = NATIVE_TARGETS[identifier]
            if target["system"] == author["system"] and target.get("architecture", architecture) == architecture:
                covered.append(identifier)
                continue
            family = "sdk" if name == "native-sdk" else "native"
            key = family, target["runner"]
            if key not in jobs:
                jobs[key] = {"os": target["runner"], "family": family, "checks": [], "targets": [],
                             "name": (f"Complement local host ({target['runner']})" if family == "native" else
                                      f"Complement local host ({family}, {target['runner']})"),
                             "node": "20" if family == "sdk" else "22", "timeout": 15 if family == "sdk" else 90}
            if name not in jobs[key]["checks"]:
                jobs[key]["checks"].append(name)
            if identifier not in jobs[key]["targets"]:
                jobs[key]["targets"].append(identifier)
        if not covered:
            raise RuntimeError(f"Author host does not cover a required target of {name}: {author}")
        local_targets.append({"check": name, "targets": covered})
    return {"version": 1, "scope": "source-acceptance", "author": author,
            "local_checks": list(names), "local_targets": local_targets,
            "ci": list(jobs.values()), "deployment_verified": False}


def report_plan(plan):
    label = {"macos": "macOS", "windows": "Windows", "linux": "Linux"}[plan["author"]["system"]]
    lines = [f"Author-local {label}: {', '.join(plan['local_checks'])}"]
    for job in plan["ci"]:
        lines.append(f"CI {', '.join(NATIVE_TARGETS[target]['label'] for target in job['targets'])}: "
                     f"{', '.join(job['checks'])} ({job['os']})")
    lines.append("Scope: source tests only; installation and deployed-system acceptance are not verified by this receipt.")
    print("\n".join(lines), flush=True)
    print("ACCEPTANCE_PLAN=" + json.dumps(plan, separators=(",", ":")), flush=True)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as stream:
            stream.write("## Acceptance responsibilities\n\n" + "\n".join("- " + line for line in lines) + "\n")


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


def changed_contract_files(head, base):
    protected = [POLICY, SIGNERS, ".github/scripts/local_acceptance.py", ".github/workflows/local-acceptance.yml",
                 ".github/workflows/local-acceptance-publish.yml"]
    return sorted(path for path in changed_paths(value("merge-base", base, head), head)
                  if path in protected or path.startswith(".github/local-acceptance/"))


def check_contract_changes(head, base, policy_ref):
    # An existing PR need not merge main merely to acquire newly added hook
    # files. It uses the protected runner. All candidate contract edits,
    # including additional checks, are blocked from authorizing themselves.
    changed = changed_contract_files(head, base)
    if changed:
        if contract_hash(head) != contract_hash(policy_ref):
            raise RuntimeError(
                "Candidate changes the protected acceptance contract. "
                f"Changed protected files: {json.dumps(changed)}. "
                f"Trusted policy: {policy_ref} ({value('rev-parse', policy_ref)}). "
                "Normal acceptance/push blocks every contract change, including added checks; "
                "the candidate cannot authorize its own check definitions. "
                f"Next: {CONTRACT_UPDATE_COMMAND}. "
                "That command requires a GitHub repository maintain/admin actor and an already enrolled signing device, "
                "runs the candidate checks, and publishes exact-head contract authorization; CI still verifies all tests. "
                "this is not a request for the end user to review code or approve this push. "
                "See CONTRIBUTING.md#acceptance-contract-changes."
            )


def github_api(path, payload=None):
    executable = shutil.which("gh") or "gh"
    command = [executable, "api", path, "--method", "POST" if payload is not None else "GET"]
    if os.name == "nt" and Path(executable).suffix.lower() in {".cmd", ".bat"}:
        raise RuntimeError("Use the native GitHub CLI gh.exe for contract-update so API arguments avoid shell expansion")
    if payload is not None:
        command += ["--input", "-"]
    try:
        result = subprocess.run(command, cwd=ROOT, input=canonical(payload) if payload is not None else None,
                                capture_output=True, timeout=60)
    except subprocess.TimeoutExpired as error:
        raise RuntimeError("GitHub contract authorization timed out after 60s. Retry contract-update; "
                           "an existing valid exact-head receipt is reused.") from error
    if result.returncode:
        raise RuntimeError("GitHub contract authorization request failed: " + result.stderr.decode(errors="replace"))
    return json.loads(result.stdout)


def require_contract_maintainer(repository, actor, api=github_api):
    permission = api(f"repos/{repository}/collaborators/{urllib.parse.quote(actor, safe='')}/permission")
    if permission.get("role_name") not in {"maintain", "admin"} and permission.get("permission") != "admin":
        raise RuntimeError(f"Contract update requires repository maintain/admin authority; {actor} has "
                           f"{permission.get('role_name', permission.get('permission'))}. "
                           "Ask an authorized repository maintainer or agent to run the printed contract-update command; "
                           "the end user need not personally review code.")


def authorization_description(head, trusted):
    # Both full hashes fit GitHub's 140-character status description limit.
    return f"v1 {contract_hash(trusted)} {contract_hash(head)}"


def authorized_policy(head, base, trusted, api=github_api):
    if not changed_contract_files(head, base) or contract_hash(head) == contract_hash(trusted):
        return trusted
    repository = load_policy(trusted)["repository"]
    if load_policy(head)["repository"] != repository:
        raise RuntimeError("Contract authorization cannot change this repository's identity")
    statuses = []
    for page in range(1, 101):
        try:
            batch = api(f"repos/{repository}/commits/{head}/statuses?per_page=100&page={page}")
        except (RuntimeError, OSError, urllib.error.HTTPError) as error:
            try:
                check_contract_changes(head, base, trusted)
            except RuntimeError as diagnostic:
                raise RuntimeError(f"{diagnostic} Authorization lookup failed: {error}") from error
            raise
        statuses += [item for item in batch if item["context"] == AUTHORIZATION_CONTEXT]
        if statuses or len(batch) < 100:
            break
    expected_url = f"https://github.com/{repository}/commit/{head}"
    if not statuses:
        check_contract_changes(head, base, trusted)
    status = statuses[0]
    if (status["state"] != "success" or status.get("description") != authorization_description(head, trusted)
            or status.get("target_url") != expected_url):
        raise RuntimeError(f"Contract authorization is stale or revoked. Next: {CONTRACT_UPDATE_COMMAND}. "
                           "This rechecks the exact head and current trusted rules; "
                           "it does not waive failed tests.")
    require_contract_maintainer(repository, status["creator"]["login"], api)
    print(f"Contract authorization by {status['creator']['login']} for {head[:12]} verified; tests remain required.", flush=True)
    return head


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


def verify_envelope(envelope, head, base, policy_ref, signers_ref=None):
    payload = base64.b64decode(envelope["payload"], validate=True)
    receipt = json.loads(payload)
    signer_id = envelope["signer"]
    registry = json.loads(tracked(signers_ref or policy_ref, SIGNERS))["signers"]
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
    required = selected(load_policy(policy_ref), changed_paths(receipt["base"], head))
    if not required:
        raise RuntimeError("Acceptance selected no actual checks; an empty contract cannot authorize a passing receipt")
    compatible = receipt.get("commands_sha256") == commands_hash(policy_ref)
    if "profile_commands_sha256" in receipt or (not compatible and "commands_sha256" in receipt):
        compatible = compatible_profile_commands(receipt, policy_ref, required)
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
    checks = receipt.get("checks", {})
    if not set(required).issubset(checks):
        raise RuntimeError(f"Acceptance check set differs from policy: need {required}")
    for name, commands in checks.items():
        if not isinstance(commands, list) or not commands:
            raise RuntimeError(f"Missing actual commands for {name}")
        for command in commands:
            if command.get("exit_code") != 0 or not command.get("command") or len(command.get("log_sha256", "")) != 64:
                raise RuntimeError(f"Incomplete/failed command in {name}")
    plan = acceptance_plan(load_policy(policy_ref), required, receipt["host"])
    if receipt.get("plan", plan) != plan:
        raise RuntimeError("Signed acceptance plan does not match the protected policy/author host")
    report_plan(plan)
    print(f"Verified {head[:12]} on {signer['platform']} ({signer_id}): {', '.join(required)}", flush=True)
    return receipt


def verify(head, base, policy_ref, note_ref=NOTE_REF, signers_ref=None):
    errors = []
    for envelope in read_note(head, note_ref)["attestations"].values():
        try:
            return verify_envelope(envelope, head, base, policy_ref, signers_ref)
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
    if not needed:
        raise RuntimeError("Acceptance selected no actual checks; keep at least one real command before contract-update")
    plan = acceptance_plan(load_policy(policy_ref), needed, host())
    report_plan(plan)
    identity = value("config", "localAcceptance.signer")
    key = Path(value("config", "localAcceptance.key")).expanduser()
    registry = json.loads(tracked(policy_ref, SIGNERS))["signers"]
    if identity not in registry or registry[identity]["platform"] != host()["system"]:
        raise RuntimeError("Enroll this host's public signing key on main before running acceptance")
    output = Path(args.output or (Path.home() / ".nexus/local-acceptance/results" / head / str(time.time_ns()))).resolve()
    if output == ROOT or ROOT in output.parents:
        raise RuntimeError("Keep acceptance outputs outside the source checkout")
    output.mkdir(parents=True, exist_ok=False)
    (output / "plan.json").write_bytes(canonical(plan))
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
               "profile_commands_sha256": profile_commands_hashes(policy_ref, needed),
               "finished_at": int(time.time()), "checks": results, "plan": plan}
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
        if changed_contract_files(head, "origin/main") and contract_hash(head) != contract_hash("origin/main"):
            try:
                contract_remote(args.remote, load_policy("origin/main")["repository"])
            except RuntimeError:
                # Local bare fixtures and unrelated remotes cannot carry a
                # GitHub maintainer's authorization for this repository.
                check_contract_changes(head, "origin/main", "origin/main")
        policy_ref = authorized_policy(head, "origin/main", "origin/main")
        try:
            verify(head, "origin/main", policy_ref, signers_ref="origin/main")
        except RuntimeError:
            if policy_ref != "origin/main":
                # The exact candidate executor was authorized by the server.
                run_candidate_checks(head, "origin/main", args.remote)
            else:
                check(argparse.Namespace(head=head, base="origin/main", policy_ref=policy_ref,
                                         output=None, publish=True, remote=args.remote))


def contract_actor(trusted):
    repository = load_policy(trusted)["repository"]
    actor = github_api("user")["login"]
    require_contract_maintainer(repository, actor)
    return repository, actor


def contract_remote(remote, repository):
    configured = value("config", "--get", f"remote.{remote}.url")
    if configured not in {f"https://github.com/{repository}.git", f"git@github.com:{repository}.git",
                          f"ssh://git@github.com/{repository}.git"}:
        raise RuntimeError(f"Contract update remote must point to {repository}. Inspect git remote -v, "
                           "then use contract-update --remote with that repository's remote name.")
    return value("remote", "get-url", "--push", remote)


def contract_pre_push(args):
    """A one-invocation hook: require authority, exact sources and a real signed receipt."""
    clean(args.head)
    repository, actor = contract_actor(args.trusted)
    updates = [line.split() for line in sys.stdin.read().splitlines() if line.strip()]
    branch = value("symbolic-ref", "HEAD")
    expected_url = contract_remote(args.remote, repository)
    already_published = (not updates and value("ls-remote", args.remote, branch).split()[:1] == [args.head])
    valid_update = len(updates) == 1 and len(updates[0]) == 4 and updates[0][:3] == [branch, args.head, branch]
    if (args.url != expected_url or branch == "refs/heads/main" or not (valid_update or already_published)):
        raise RuntimeError("Contract update must push exactly the checked-out non-main branch to its protected GitHub repository")
    # Recheck authority and the signed input immediately at the actual push.
    verify(args.head, args.trusted, args.head, signers_ref=args.trusted)
    print(f"Exact-head contract push authorized for {actor}; native CI is still required.", flush=True)


def contract_update(args):
    head = value("rev-parse", "HEAD")
    clean(head)
    git("fetch", "--no-tags", args.remote, "+refs/heads/main:refs/remotes/origin/main")
    trusted = value("rev-parse", "origin/main")
    repository, actor = contract_actor(trusted)
    if load_policy(head)["repository"] != repository:
        raise RuntimeError("Contract authorization cannot change this repository's identity")
    branch = value("symbolic-ref", "--short", "HEAD")
    contract_remote(args.remote, repository)
    if branch == "main":
        raise RuntimeError("Use a non-main branch for contract-update")
    if git("merge-base", "--is-ancestor", trusted, head, check=False).returncode:
        raise RuntimeError("Trusted main advanced. Integrate origin/main while preserving other changes, then rerun contract-update.")
    if not changed_contract_files(head, trusted):
        raise RuntimeError("No protected contract files changed. Next: git push --set-upstream origin HEAD")
    signer = value("config", "localAcceptance.signer")
    if signer not in json.loads(tracked(trusted, SIGNERS))["signers"]:
        raise RuntimeError("Use an already enrolled signing device to authorize a contract update, including new device enrollment.")
    print(f"Contract update by {actor}: head {head}, trusted main {trusted}; all selected local checks remain required.", flush=True)
    fetched = git("fetch", "--no-tags", args.remote, f"+{NOTE_REF}:{NOTE_REF}", check=False)
    if fetched.returncode and "couldn't find remote ref" not in fetched.stderr.decode(errors="replace"):
        raise RuntimeError(fetched.stderr.decode(errors="replace"))
    try:
        verify(head, trusted, head, signers_ref=trusted)
    except RuntimeError:
        run_candidate_checks(head, trusted, args.remote, args.output)
    with tempfile.TemporaryDirectory(prefix="scode-contract-push-") as temporary:
        hook = Path(temporary) / "pre-push"
        interpreter = shlex.quote(sys.executable.replace("\\", "/"))
        helper = shlex.quote(str(Path(__file__).resolve()).replace("\\", "/"))
        hook.write_text(f'#!/bin/sh\nexec {interpreter} {helper} contract-pre-push --head {head} --trusted {trusted} "$@"\n',
                        encoding="utf-8", newline="\n")
        hook.chmod(0o755)
        git("-c", f"core.hooksPath={temporary}", "push", "--set-upstream", args.remote,
            f"refs/heads/{branch}:refs/heads/{branch}")
    try:
        status = github_api(f"repos/{repository}/statuses/{head}", {
            "state": "success", "context": AUTHORIZATION_CONTEXT,
            "description": authorization_description(head, trusted),
            "target_url": f"https://github.com/{repository}/commit/{head}",
        })
        if status["creator"]["login"] != actor:
            raise RuntimeError("GitHub authorization actor changed during publication")
    except RuntimeError as error:
        raise RuntimeError("Branch was pushed, but contract authorization is incomplete. Rerun contract-update; "
                           "the existing exact-head signed receipt will be reused. " + str(error)) from error
    print(f"Contract authorization published for {head}. Local acceptance and all complementary CI checks must still pass.\n"
          f"Next: gh pr create --draft --base main --head {branch} (or inspect the existing PR).", flush=True)


def run_candidate_checks(head, trusted, remote, output=None):
    # A contract update can change run_command itself. Execute that exact
    # checked-out runner after authority verification, not this older copy.
    command = [sys.executable, str(ROOT / ".github/scripts/local_acceptance.py"), "check",
               "--head", head, "--base", trusted, "--policy-ref", head, "--publish", "--remote", remote]
    if output:
        command += ["--output", output]
    subprocess.run(command, cwd=ROOT, check=True)
    git("fetch", "--no-tags", remote, f"+{NOTE_REF}:{NOTE_REF}")
    verify(head, trusted, head, signers_ref=trusted)


def main():
    global ROOT
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="operation", required=True)
    for name in ["check", "verify", "plan"]:
        command = sub.add_parser(name)
        command.add_argument("--head")
        command.add_argument("--base", default="origin/main")
        command.add_argument("--policy-ref", default="origin/main")
        if name == "plan":
            command.add_argument("--host", choices=["linux", "macos", "windows"])
            command.add_argument("--architecture")
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
    command = sub.add_parser("contract-update", help="Run and publish an exact-head contract update as a repository maintainer")
    command.add_argument("--remote", default="origin")
    command.add_argument("--output")
    command = sub.add_parser("contract-pre-push", help=argparse.SUPPRESS)
    command.add_argument("--head", required=True)
    command.add_argument("--trusted", required=True)
    command.add_argument("remote")
    command.add_argument("url")
    args = parser.parse_args()
    try:
        # Installed runners live outside a worktree. Their caller's checkout,
        # not the installation directory, supplies the exact tested sources.
        ROOT = Path(value("rev-parse", "--show-toplevel", root=Path.cwd()))
        if args.operation == "plan":
            head = value("rev-parse", args.head or "HEAD")
            base = value("merge-base", args.base, head)
            author = host()
            if args.host:
                author["system"] = args.host
            if args.architecture:
                author["architecture"] = args.architecture
            report_plan(acceptance_plan(load_policy(args.policy_ref),
                        selected(load_policy(args.policy_ref), changed_paths(base, head)), author))
            print("Preview only: a plan is not a successful or signed test result.")
        elif args.operation == "verify":
            head = value("rev-parse", args.head or "HEAD")
            trusted = args.policy_ref
            policy_ref = authorized_policy(head, args.base, trusted) if trusted == "origin/main" else trusted
            verify(head, args.base, policy_ref, signers_ref=trusted)
        elif args.operation == "check":
            if args.policy_ref == "origin/main":
                args.policy_ref = authorized_policy(value("rev-parse", args.head or "HEAD"), args.base, args.policy_ref)
            check(args)
        elif args.operation == "install":
            install(args)
        elif args.operation == "contract-update":
            contract_update(args)
        elif args.operation == "contract-pre-push":
            contract_pre_push(args)
        else:
            pre_push(args)
    except (RuntimeError, subprocess.CalledProcessError, OSError, ValueError, KeyError) as error:
        print(f"Local acceptance blocked: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
