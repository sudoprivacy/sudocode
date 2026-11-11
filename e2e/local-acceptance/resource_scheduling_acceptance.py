"""Exercise resource-only plan/evidence evolution through real Git, SSH and CLI processes."""
import argparse
import ast
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys


def verify_command_completion(output, source):
    """Compare original/new supervisors with real child exit and error results."""
    import importlib.util
    import psutil

    output.mkdir(parents=True, exist_ok=False)
    wait = """                try:
                    process.wait(timeout=0.15)
                except subprocess.TimeoutExpired:
                    pass"""
    assert source.count(wait) == 1
    original = output / "original-supervisor.py"
    original.write_text(source.replace(wait, "                time.sleep(0.15)", 1), encoding="utf-8")
    version = subprocess.check_output(["git", "--version"])
    observations = []
    for name, path in [("original", original), ("completion", output.parent / "trusted-verifier.py")]:
        spec = importlib.util.spec_from_file_location("completion_" + name, path)
        helper = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(helper)
        log = output / f"{name}-short.log"
        result = helper.run_command(["git", "--version"], output, log)
        assert result["exit_code"] == 0 and log.read_bytes() == version
        rows = [result["elapsed_seconds"]]
        code = ('import psutil,sys; '
                'assert psutil.Process().nice()==' + str(psutil.Process().nice()) + '; '
                'sys.stdout.buffer.write(b"original-child-output\\x00\\n")')
        priority = output / f"{name}-priority.log"
        helper.run_command([sys.executable, "-c", code], output, priority)
        assert priority.read_bytes() == b"original-child-output\x00\n"
        long_log = output / f"{name}-pending.log"
        pending = helper.run_command([sys.executable, "-c", 'import time; time.sleep(0.35); print("finished")'], output, long_log)
        assert pending["exit_code"] == 0 and pending["elapsed_seconds"] >= 0.35
        assert long_log.read_bytes().strip() == b"finished"
        failure = output / f"{name}-failure.log"
        try:
            helper.run_command([sys.executable, "-c", 'import sys; print("actual exit 7"); sys.exit(7)'], output, failure)
        except RuntimeError:
            failed = json.loads(failure.with_suffix(".command.json").read_bytes())
            assert failed["exit_code"] == 7 and failure.read_bytes().strip() == b"actual exit 7"
            assert failed["log_sha256"] == hashlib.sha256(failure.read_bytes()).hexdigest()
        else:
            raise AssertionError("Completion wait discarded an actual child failure")
        observations.append({"supervisor": name, "short_command_seconds": rows})
    return observations


def verify_resource_scheduling(output, source, command):
    output.mkdir(parents=True, exist_ok=False)
    work = output / "author"
    work.mkdir()

    def run(*args, **kwargs):
        return command(*args, cwd=work, **kwargs)

    def write(name, content):
        path = work / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8", newline="\n")
        return path

    def commit(message):
        run("git", "add", ".")
        run("git", "commit", "-m", message)
        return run("git", "rev-parse", "HEAD")

    adaptive = (source / ".github/scripts/local_acceptance.py").read_text(encoding="utf-8")
    function = next(node for node in ast.parse(adaptive).body if isinstance(node, ast.FunctionDef) and node.name == "run_command")
    nodes = function.body
    first = next(index for index, node in enumerate(nodes) if isinstance(node, ast.Assign)
                 and any(isinstance(target, ast.Name) and target.id == "jobs" for target in node.targets))
    lines = adaptive.splitlines(keepends=True)
    fragment = "".join(lines[function.lineno - 1:function.end_lineno])
    start, end = nodes[first].lineno - function.lineno, nodes[first + 2].end_lineno - function.lineno + 1
    local = fragment.splitlines(keepends=True)
    del local[start:end]
    serial_function = "".join(local)
    assert serial_function.count('"CARGO_BUILD_JOBS": str(jobs)') == 1
    serial_function = serial_function.replace('"CARGO_BUILD_JOBS": str(jobs)', '"CARGO_BUILD_JOBS": "1"')
    serial = "".join(lines[:function.lineno - 1]) + serial_function + "".join(lines[function.end_lineno:])
    serial_completion = serial
    completion_wait = """                try:
                    process.wait(timeout=0.15)
                except subprocess.TimeoutExpired:
                    pass"""
    assert serial.count(completion_wait) == 1
    serial = serial.replace(completion_wait, "                time.sleep(0.15)", 1)
    dispatcher = (source / ".github/local-acceptance/cross_platform.py").read_text(encoding="utf-8")
    execute = next(node for node in ast.parse(dispatcher).body if isinstance(node, ast.FunctionDef) and node.name == "execute")
    lines = dispatcher.splitlines(keepends=True)
    serial_dispatch = "".join(lines[:execute.lineno - 1]) + '''def execute(command, cwd, log, env=None):
    jobs = (os.cpu_count() or 1) if os.environ.get("GITHUB_ACTIONS") == "true" else 1
    return helper.run_command(command, cwd, log, {**(env or {}), "CARGO_BUILD_JOBS": str(jobs)})
''' + "".join(lines[execute.end_lineno:])
    run("git", "init", "--initial-branch=main")
    run("git", "config", "user.name", "Resource acceptance fixture")
    run("git", "config", "user.email", "fixture@example.invalid")
    run("git", "config", "core.autocrlf", "false")
    key = output / "fixture-key"
    run("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", key)
    helper = write(".github/scripts/local_acceptance.py", serial)
    write(".github/local-acceptance/cross_platform.py", serial_dispatch)
    write(".github/local-acceptance/Dockerfile", "FROM scratch\n")
    write(".github/workflows/local-acceptance.yml", "# Private metadata fixture\n")
    write(".github/workflows/local-acceptance-publish.yml", "# Private metadata fixture\n")
    checks = '''import sys
def run(name, root, output, base, execute):
    output.mkdir(parents=True, exist_ok=True)
    return [execute([sys.executable, str(root / "canary.py"), name,
        "--test-threads", "8", "--retries", "0", "--timeout", "10"], root, output / "000.log")]
'''
    write(".github/local-acceptance/checks.py", checks)
    write("canary.py", '''import json,os,sys
from pathlib import Path
assert Path("input.txt").read_text()=="pass"
assert sys.argv[2:]==["--test-threads","8","--retries","0","--timeout","10"]
with Path(os.environ["RESOURCE_CANARY_LOG"]).open("a") as log:
    log.write(json.dumps({"name":sys.argv[1],"jobs":os.environ["CARGO_BUILD_JOBS"]})+"\\n")
print("actual resource fixture command passed")
''')
    names = ["preflight", "native-workspace"]
    write("spec/local_acceptance.json", json.dumps({"version": 1, "repository": "fixture/resource-scheduling",
          "checks": {name: {"include": ["input.txt"]} for name in names}}))
    host = json.loads(run(sys.executable, "-c", "import json,runpy; h=runpy.run_path('.github/scripts/local_acceptance.py'); print(json.dumps(h['host']()))"))
    write("spec/local_acceptance_signers.json", json.dumps({"version": 1, "signers": {
        "fixture": {"platform": host["system"], "public_key": key.with_suffix(".pub").read_text().strip()}}}))
    write("input.txt", "before")
    run("git", "config", "localAcceptance.signer", "fixture")
    run("git", "config", "localAcceptance.key", key)
    protected = commit("Original serial compiler fixture policy")
    run("git", "checkout", "-b", "source")
    write("input.txt", "pass")
    head = commit("Original frozen native fixture input")
    original = output / "original"
    ledger = output / "actual-calls.jsonl"
    driver = '''import argparse,json,os,runpy,sys
from pathlib import Path
h=runpy.run_path('.github/scripts/local_acceptance.py')
head,policy,out,key,ledger=sys.argv[1:]
os.environ['RESOURCE_CANARY_LOG']=ledger
rules=h['load_policy'](policy)
names=list(rules['checks'])
host=h['host']()
payload={'version':1,'kind':'execution-plan','repository':rules['repository'],'head':head,
    'tree':h['value']('rev-parse',head+'^{tree}'),'base':h['value']('merge-base',head,policy),
    'policy':policy,'contract_sha256':h['contract_hash'](policy),'host':host,
    'plan':h['acceptance_plan'](rules,names,host),'checks':{}}
plan=h['sign_receipt'](payload,Path(out).parent/'original-plan','fixture',Path(key),namespace=h['NAMESPACE']+'-plan')
h['verify_execution_plan'](plan,head,policy,policy)
h['check'](argparse.Namespace(head=head,base=policy,policy_ref=policy,signers_ref=policy,
    output=out,resume_from=None,publish=False,remote='origin',execution_plan=plan))
Path(out).parent.joinpath('plan-note.json').write_text(json.dumps({'version':1,'attestations':{'fixture':plan}}))
envelope=json.loads(Path(out).joinpath('envelope.json').read_text())
Path(out).parent.joinpath('full-note.json').write_text(json.dumps({'version':1,'attestations':{'fixture':envelope}}))
'''
    run(sys.executable, "-c", driver, head, protected, original, key, ledger)
    assert len(ledger.read_text().splitlines()) == 2
    assert all(json.loads(row)["jobs"] == "1" for row in ledger.read_text().splitlines())
    for namespace, filename in [("local-acceptance", "full-note.json"), ("local-acceptance-plan", "plan-note.json")]:
        run("git", "notes", "--ref=" + namespace, "add", "-F", output / filename, head)
    original_files = {str(path.relative_to(original)): hashlib.sha256(path.read_bytes()).hexdigest()
                      for path in original.rglob("*") if path.is_file()}
    notes = {namespace: run("git", "notes", "--ref=" + namespace, "show", head)
             for namespace in ["local-acceptance", "local-acceptance-plan"]}
    run("git", "checkout", "main")
    write(".github/scripts/local_acceptance.py", adaptive)
    write(".github/local-acceptance/cross_platform.py", dispatcher)
    current = commit("Evolve compiler budget without changing commands or checks")
    trusted = output / "trusted-verifier.py"
    trusted.write_text(adaptive, encoding="utf-8")
    completion_observations = verify_command_completion(output / "command-completion", adaptive)
    plan_read = '''import runpy,sys
from pathlib import Path
h=runpy.run_path(sys.argv[1]); h['read_execution_plan'].__globals__['ROOT']=Path.cwd()
h['read_execution_plan'](sys.argv[2],sys.argv[3],sys.argv[3],signers_ref=sys.argv[3])
'''
    plan_output = run(sys.executable, "-c", plan_read, trusted, head, current)
    assert "LOCAL_ACCEPTANCE_PLAN_RESOURCE_EQUIVALENCE=" in plan_output
    full_output = run(sys.executable, trusted, "verify", "--head", head, "--base", current, "--policy-ref", current)
    assert "LOCAL_ACCEPTANCE_RESOURCE_EQUIVALENCE=" in full_output
    assert f"Verified {head[:12]}" in full_output
    # Completion notification alone must also preserve the original signed
    # plan and full result, without requiring a compiler-budget transition.
    run("git", "checkout", "-b", "completion-only", protected)
    write(".github/scripts/local_acceptance.py", serial_completion)
    completion_only = commit("Change only command-completion waiting")
    wait_plan = run(sys.executable, "-c", plan_read, trusted, head, completion_only)
    assert "LOCAL_ACCEPTANCE_PLAN_RESOURCE_EQUIVALENCE=" in wait_plan
    wait_full = run(sys.executable, trusted, "verify", "--head", head, "--base", completion_only, "--policy-ref", completion_only)
    assert f"Verified {head[:12]}" in wait_full
    # Disabling the new proof recreates the original refusal, with valid SSH
    # evidence and the same immutable source/plan. It cannot manufacture a pass.
    legacy = adaptive.replace('return {name for name in names if previous[name] == current[name]}', 'return set()')
    assert legacy != adaptive
    legacy_path = output / "old-behavior-verifier.py"
    legacy_path.write_text(legacy, encoding="utf-8")
    run(sys.executable, "-c", plan_read, legacy_path, head, current, success=False, error="Execution plan commands or native dispatcher changed")
    records = []
    helper_path = ".github/scripts/local_acceptance.py"
    dispatch_path = ".github/local-acceptance/cross_platform.py"

    def executor_change(before, after):
        lines = adaptive.splitlines(keepends=True)
        function = next(node for node in ast.parse(adaptive).body
                        if isinstance(node, ast.FunctionDef) and node.name == "run_command")
        original = "".join(lines[function.lineno - 1:function.end_lineno])
        assert original.count(before) == 1
        changed = original.replace(before, after, 1)
        return "".join(lines[:function.lineno - 1]) + changed + "".join(lines[function.end_lineno:])

    mutations = [
        ("test-pool", ".github/local-acceptance/checks.py", checks.replace('"--test-threads", "8"', '"--test-threads", "1"')),
        ("test-retries", ".github/local-acceptance/checks.py", checks.replace('"--retries", "0"', '"--retries", "1"')),
        ("test-deadline", ".github/local-acceptance/checks.py", checks.replace('"--timeout", "10"', '"--timeout", "11"')),
        ("runtime-env", helper_path, executor_change('"CARGO_INCREMENTAL": "0"', '"CARGO_INCREMENTAL": "1"')),
        ("credential-isolation", helper_path, executor_change('if key.upper() not in {"GH_TOKEN", "GITHUB_TOKEN"}', 'if key not in {"GH_TOKEN", "GITHUB_TOKEN"}')),
        ("unknown-budget", helper_path, executor_change('jobs = max(1, min((os.cpu_count()', 'jobs = max(2, min((os.cpu_count()')),
        ("budget-leak", helper_path, executor_change('    environment.update({"CARGO_BUILD_JOBS": str(jobs)', '    environment["SCODE_MODEL"] = str(jobs)\n    environment.update({"CARGO_BUILD_JOBS": str(jobs)')),
        ("shadow-budget-call", helper_path, adaptive.replace('\ndef run_command(', '\ndef max(*args):\n    raise RuntimeError("UNKNOWN_RESOURCE_EFFECT")\n\ndef run_command(', 1)),
        ("executor-comment", helper_path, executor_change("    import psutil\n", "    import psutil\n    # Changed executor without a resource transition.\n")),
        ("dispatcher-comment", dispatch_path, dispatcher + "\n# Changed native dispatch.\n"),
        ("dispatcher-env", dispatch_path, dispatcher.replace('"CARGO_BUILD_JOBS": str(os.cpu_count() or 1)', '"CARGO_BUILD_JOBS": str(os.cpu_count() or 1), "SCODE_MODEL": "changed"', 1)),
        ("dispatcher-state", dispatch_path, 'UNKNOWN_RESOURCE_STATE = 1\n' + dispatcher),
        ("image", ".github/local-acceptance/Dockerfile", "FROM scratch\nENV DIFFERENT_IMAGE=1\n"),
        ("workflow", ".github/workflows/local-acceptance.yml", "# Different native workflow\n"),
        ("completion-interval", helper_path, executor_change("process.wait(timeout=0.15)", "process.wait(timeout=0.16)")),
        ("completion-catch", helper_path, executor_change("except subprocess.TimeoutExpired:", "except Exception:")),
        ("completion-effect", helper_path, executor_change("process.wait(timeout=0.15)", "process.wait(timeout=0.15)\n                    environment.clear()")),
        ("shadow-wait-module", helper_path, adaptive.replace('\ndef run_command(', '\ndef subprocess(*args):\n    raise RuntimeError("UNKNOWN_WAIT_EFFECT")\n\ndef run_command(', 1)),
    ]
    for index, (name, filename, content) in enumerate(mutations):
        run("git", "checkout", "-b", f"resource-control-{index}", current)
        assert (work / filename).read_text(encoding="utf-8") != content, name
        write(filename, content)
        changed = commit("Refuse " + name)
        run(sys.executable, trusted, "verify", "--head", head, "--base", changed, "--policy-ref", changed, success=False)
        records.append({"case": name, "refused_policy": changed})
    run("git", "checkout", "source")
    resumed = output / "resumed"
    run(sys.executable, trusted, "check", "--head", head, "--base", current, "--policy-ref", current,
        "--resume-from", original, "--output", resumed)
    fresh = json.loads((resumed / "receipt.json").read_text())
    old = json.loads((original / "receipt.json").read_text())
    assert fresh["checks"] == old["checks"]
    assert fresh["timing"]["first_recorded_started_at"] == old["timing"]["first_recorded_started_at"]
    assert len(ledger.read_text().splitlines()) == 2, "Resource migration reran original commands"
    for namespace, note in notes.items():
        assert run("git", "notes", "--ref=" + namespace, "show", head) == note
    assert {str(path.relative_to(original)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in original.rglob("*") if path.is_file()} == original_files
    result = {"status": "PASS", "original_head": head, "original_policy": protected,
              "current_policy": current, "original_real_commands": 2, "migration_real_commands": 0,
              "mutation_refusals": records, "original_notes_signatures_logs_and_first_clock_preserved": True,
              "completion_only_plan_and_full_preserved": True, "completion_observations": completion_observations,
              "scope": "Private Git/SSH/CLI plan/full/resume controls. No product or missing-platform acceptance."}
    (output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result), flush=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--source", type=Path, default=Path(__file__).resolve().parents[2])
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    private_home = args.output / "home"
    private_home.mkdir()
    names = subprocess.check_output(["git", "rev-parse", "--local-env-vars"], text=True).splitlines()
    environment = {key: value for key, value in os.environ.items()
                   if key not in names and not key.startswith(("GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"))
                   and key.upper() not in {"GH_TOKEN", "GITHUB_TOKEN", "SUDOCODE_ACCEPTANCE_HOST_LEASE"}}
    environment.update(HOME=str(private_home), USERPROFILE=str(private_home), PYTHONIOENCODING="utf-8",
                       SUDOCODE_ACCEPTANCE_HOST_LEASE="", SCODE_ACCEPTANCE_BUILD_JOBS="1")
    commands = []

    def command(*argv, cwd, success=True, error=None, stderr=False):
        result = subprocess.run(list(map(str, argv)), cwd=cwd, env=environment, capture_output=True)
        index = len(commands)
        (args.output / f"{index:03}.stdout").write_bytes(result.stdout)
        (args.output / f"{index:03}.stderr").write_bytes(result.stderr)
        commands.append({"argv": list(map(str, argv)), "exit_code": result.returncode})
        (args.output / "commands.json").write_text(json.dumps(commands, indent=2) + "\n")
        if (result.returncode == 0) != success:
            raise RuntimeError(f"Unexpected exit {result.returncode}: {argv}\n{result.stdout.decode(errors='replace')}\n{result.stderr.decode(errors='replace')}")
        if error is not None:
            assert error in result.stderr.decode(errors="replace"), result.stderr
        return (result.stderr if stderr else result.stdout).decode("utf-8", errors="replace").strip()

    verify_resource_scheduling(args.output / "fixture", args.source.resolve(), command)
