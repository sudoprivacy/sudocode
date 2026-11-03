"""Change real Git refs during a signed CLI check and verify its frozen inputs."""

import json
import subprocess
import sys


def verify_frozen_refs(directory, work, environment):
    fixture = directory / "frozen-refs"
    fixture.mkdir()
    clone = fixture / "repository"

    def command(*args, cwd=clone):
        result = subprocess.run(list(map(str, args)), cwd=cwd, env=environment,
            capture_output=True, text=True, encoding="utf-8", errors="replace")
        assert result.returncode == 0, result.stdout + result.stderr
        return result.stdout.strip()

    remote = command("git", "remote", "get-url", "origin", cwd=work)
    command("git", "clone", remote, clone, cwd=fixture)
    for key in ["user.name", "user.email", "localAcceptance.signer", "localAcceptance.key"]:
        command("git", "config", key, command("git", "config", "--get", key, cwd=work))
    protected = command("git", "rev-parse", "origin/main")
    command("git", "checkout", "-b", "frozen-candidate", protected)
    (clone / "ref-probe.txt").write_text("Trigger the real acceptance process.\n")
    command("git", "add", ".")
    command("git", "commit", "-m", "A candidate checked against frozen policy")
    head = command("git", "rev-parse", "HEAD")
    command("git", "checkout", "-b", "moved-policy", protected)
    policy = clone / "spec/local_acceptance.json"
    changed = json.loads(policy.read_text())
    changed["checks"]["unrun-ref-change"] = {"include": ["**"]}
    policy.write_text(json.dumps(changed))
    command("git", "add", ".")
    command("git", "commit", "-m", "Concurrent protected policy update")
    moved = command("git", "rev-parse", "HEAD")
    command("git", "checkout", "frozen-candidate")
    command("git", "update-ref", "refs/tags/fixture-policy", protected)
    output = fixture / "receipt"
    worker = fixture / "run_check.py"
    worker.write_text(f'MOVED={moved!r}\nOUTPUT={str(output)!r}\n'
        f'PROTECTED={protected!r}\nCANDIDATE={head!r}\n'
        + '''import argparse,importlib.util,json,subprocess
from pathlib import Path
root=Path.cwd()
spec=importlib.util.spec_from_file_location('fixture_executor',root/'.github/scripts/local_acceptance.py')
module=importlib.util.module_from_spec(spec);spec.loader.exec_module(module)
original=module.run_command
moved=False
def execute(*args,**kwargs):
    global moved
    result=original(*args,**kwargs)
    if not moved:
        for ref in ['refs/tags/fixture-policy','refs/remotes/origin/main']:
            subprocess.run(['git','update-ref',ref,MOVED],cwd=root,check=True)
        moved=True
    return result
module.run_command=execute
module.check(argparse.Namespace(head='HEAD',base='origin/main',policy_ref='refs/tags/fixture-policy',
    output=OUTPUT,publish=False,remote='origin'))
assert moved
receipt=json.loads((Path(OUTPUT)/'receipt.json').read_text())
assert receipt['commands_ref']==PROTECTED,receipt
assert receipt['base']==PROTECTED and receipt['head']==CANDIDATE,receipt
assert set(receipt['checks'])=={'real-process'},receipt
assert all(c['exit_code']==0 for rows in receipt['checks'].values() for c in rows)
for ref in ['refs/tags/fixture-policy','refs/remotes/origin/main']:
    assert subprocess.check_output(['git','rev-parse',ref],cwd=root,text=True).strip()==MOVED
print('FROZEN_SIGNED_CLI_INPUTS_PASS',flush=True)
''', encoding="utf-8")
    observed = command(sys.executable, worker)
    assert "FROZEN_SIGNED_CLI_INPUTS_PASS" in observed, observed
    print("PASS: real signed CLI keeps its policy/base after concurrent Git ref changes")
