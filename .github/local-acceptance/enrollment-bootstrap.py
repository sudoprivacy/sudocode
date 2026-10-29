"""One reviewed enrollment only; not a general acceptance bypass."""

import ast
import importlib.util
import json
import os
from pathlib import Path
import re
import subprocess
import urllib.request

REPOSITORY = 'sudoprivacy/sudocode'
PR = 1122
HEAD = '0051214062fd7f5614f18ec0b2a450c3f817ce3b'
BASE = '59079832bbbb850b9b6239e46ec7d8e7bbc3bed6'
REVIEW = 5700303233
SIGNER = 'elfenlieds7-macos-local'
ADMINISTRATOR = 'elfenlieds7'
MARKER = '<!-- scode-macos-enrollment-bootstrap -->'
BLOBS = {
    'spec/local_acceptance_signers.json': '12281094f3573682dadf505f0eff221b87473ac0',
    'CONTRIBUTING.md': 'ac2b4cde3bbf66c060e3ceebf753892e065f3ab1',
}
PUBLIC_KEY = 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGRh8eWUbTt1iTxrYf24Hh+JYLY7p/Jl8/3mwmMfTlzc'
ROOT = Path.cwd()


def git(*args):
    return subprocess.check_output(['git', *args], cwd=ROOT, text=True).strip()


def api(path, body=None):
    request = urllib.request.Request(
        'https://api.github.com/' + path,
        data=None if body is None else json.dumps(body).encode(),
        headers={'Authorization': 'Bearer ' + os.environ['GH_TOKEN'],
                 'Accept': 'application/vnd.github+json', 'Content-Type': 'application/json'},
        method='GET' if body is None else 'POST')
    with urllib.request.urlopen(request, timeout=60) as response:
        return json.load(response)


def pages(path, field=None):
    result = []
    for page in range(1, 11):
        value = api(path + ('&' if '?' in path else '?') + f'per_page=100&page={page}')
        rows = value[field] if field else value
        result.extend(rows)
        if len(rows) < 100:
            return result
    raise RuntimeError('Bounded bootstrap metadata pagination exceeded')


def main():
    assert os.environ['GITHUB_REPOSITORY'] == REPOSITORY
    assert os.environ['GITHUB_ACTOR'] == ADMINISTRATOR
    assert git('rev-parse', 'HEAD') == BASE, 'Execute only the frozen protected baseline checkout'
    pull = api(f'repos/{REPOSITORY}/pulls/{PR}')
    assert pull['state'] == 'open' and not pull['draft']
    assert pull['head']['sha'] == HEAD and pull['head']['repo']['full_name'] == REPOSITORY
    review = api(f'repos/{REPOSITORY}/pulls/{PR}/reviews/{REVIEW}')
    assert review['user']['login'] == ADMINISTRATOR and review['commit_id'] == HEAD
    assert review['state'] in ['COMMENTED', 'APPROVED'] and MARKER in review['body']
    role = api(f'repos/{REPOSITORY}/collaborators/{ADMINISTRATOR}/permission')
    assert role['permission'] == 'admin'
    subprocess.run(['git', 'fetch', '--no-tags', 'origin', HEAD,
                    '+refs/notes/local-acceptance:refs/notes/local-acceptance'], check=True)
    assert git('diff', '--name-only', BASE, HEAD).splitlines() == sorted(BLOBS)
    for path, blob in BLOBS.items():
        assert git('rev-parse', HEAD + ':' + path) == blob
    original = json.loads(git('show', BASE + ':spec/local_acceptance_signers.json'))
    expected = json.loads(json.dumps(original))
    assert SIGNER not in expected['signers']
    expected['signers'][SIGNER] = dict(actor=ADMINISTRATOR, platform='macos', public_key=PUBLIC_KEY)
    assert json.loads(git('show', HEAD + ':spec/local_acceptance_signers.json')) == expected

    # Import only protected code. No candidate Python, Rust, hooks or workflow is executed.
    spec = importlib.util.spec_from_file_location('protected_acceptance', ROOT / '.github/scripts/local_acceptance.py')
    acceptance = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(acceptance)
    assert acceptance.contract_hash(BASE) == acceptance.contract_hash(HEAD)
    receipt = acceptance.verify(HEAD, BASE, HEAD)
    assert receipt['host']['system'] == 'macos'
    assert set(receipt['checks']) == {'preflight', 'guard-contract'}
    assert [r['command'][1:] for r in receipt['checks']['preflight']] == [
        ['.github/scripts/check_doc_source_of_truth.py'],
        ['.github/scripts/check_pr_history.py', BASE, 'HEAD'],
    ]
    assert [r['command'][1:] for r in receipt['checks']['guard-contract']] == [['e2e/local-acceptance/run.py']]
    kind = os.environ['BOOTSTRAP_TARGET_KIND']
    assert kind in ['pull_request', 'merge_group']
    target = HEAD
    if kind == 'merge_group':
        query = ('query { repository(owner:"sudoprivacy",name:"sudocode") { pullRequest(number:1122) '
                 '{ mergeQueueEntry { headCommit { oid } baseCommit { oid } } } } }')
        queue = json.loads(subprocess.check_output(['gh', 'api', 'graphql', '-f', 'query=' + query], text=True))
        entry = queue['data']['repository']['pullRequest']['mergeQueueEntry']
        assert entry, 'Enrollment is not in its normal merge queue'
        target, queue_base = entry['headCommit']['oid'], entry['baseCommit']['oid']
        assert re.fullmatch('[a-f0-9]{40}', target) and re.fullmatch('[a-f0-9]{40}', queue_base)
        subprocess.run(['git', 'fetch', '--no-tags', 'origin', target, queue_base], check=True)
        subprocess.run(['git', 'merge-base', '--is-ancestor', HEAD, target], check=True)
        assert git('diff', '--name-only', queue_base, target).splitlines() == sorted(BLOBS), 'No other PR or source change may share this enrollment bootstrap'
        # Main may advance its scheduling/verification code while this data-only
        # enrollment waits. Preserve that code and every other registered device.
        prior_registry = json.loads(git('show', queue_base + ':spec/local_acceptance_signers.json'))
        assert SIGNER not in prior_registry['signers']
        prior_registry['signers'][SIGNER] = expected['signers'][SIGNER]
        assert json.loads(git('show', target + ':spec/local_acceptance_signers.json')) == prior_registry
        def changed_document_lines(older, newer):
            diff = git('diff', '--no-color', '--no-ext-diff', '--unified=0', older, newer, '--', 'CONTRIBUTING.md')
            return [line for line in diff.splitlines() if line.startswith(('+', '-')) and not line.startswith(('+++', '---'))]
        assert changed_document_lines(BASE, HEAD) == changed_document_lines(queue_base, target)
        assert acceptance.contract_hash(queue_base) == acceptance.contract_hash(target)
        # Honor main's unchanged-command receipt compatibility, without executing
        # a newer or candidate runner in this write-enabled operation.
        for path in ['.github/local-acceptance/checks.py', '.github/local-acceptance/Dockerfile']:
            assert git('rev-parse', BASE + ':' + path) == git('rev-parse', queue_base + ':' + path)
        def executor(ref):
            source = git('show', ref + ':.github/scripts/local_acceptance.py')
            node = next(n for n in ast.parse(source).body if isinstance(n, ast.FunctionDef) and n.name == 'run_command')
            return '\n'.join(source.splitlines()[node.lineno - 1:node.end_lineno])
        assert executor(BASE) == executor(queue_base)
        required = acceptance.selected(acceptance.load_policy(queue_base), acceptance.changed_paths(BASE, HEAD))
        assert set(required).issubset(receipt['checks'])
        subprocess.run(['python3', '.github/scripts/check_pr_history.py', queue_base, target], check=True)

    # Every ordinary test must finish successfully. The sole initialization
    # exception is the protected verifier not yet knowing this reviewed key.
    runs = pages(f'repos/{REPOSITORY}/actions/runs?head_sha={target}&event={kind}', 'workflow_runs')
    latest = {}
    for run in runs:
        latest.setdefault(run['path'].split('@')[0], run)
    for path in ['.github/workflows/rust-ci.yml', '.github/workflows/pr-history.yml']:
        assert latest[path]['status'] == 'completed' and latest[path]['conclusion'] == 'success', (path, latest[path]['status'], latest[path]['conclusion'])
    ordinary = latest['.github/workflows/local-acceptance.yml']
    assert ordinary['status'] == 'completed' and ordinary['conclusion'] == 'failure'
    jobs = pages(f'repos/{REPOSITORY}/actions/runs/{ordinary["id"]}/jobs', 'jobs')
    proof = next(j for j in jobs if j['name'] == 'Verify local acceptance')
    assert proof['conclusion'] == 'failure'
    logs = subprocess.check_output(['gh', 'api', f'repos/{REPOSITORY}/actions/jobs/{proof["id"]}/logs'], text=True)
    assert 'Unenrolled acceptance signer: ' + SIGNER in logs, 'Do not hide a different acceptance failure'
    assert all(j['conclusion'] in ['success', 'skipped'] or j['name'] in ['Verify local acceptance', 'Cross-platform acceptance result'] for j in jobs)
    statuses = pages(f'repos/{REPOSITORY}/commits/{target}/statuses')
    prior = next(s for s in statuses if s['context'] == 'Local acceptance')
    assert prior['state'] == 'failure', 'Bootstrap must follow the ordinary initialization refusal'
    prefix = f'https://github.com/{REPOSITORY}/actions/runs/'
    assert prior['target_url'].startswith(prefix)
    publisher_id = prior['target_url'][len(prefix):]
    assert publisher_id.isdecimal()
    publisher = api(f'repos/{REPOSITORY}/actions/runs/{publisher_id}')
    assert publisher['path'].split('@')[0] == '.github/workflows/local-acceptance-publish.yml'
    assert publisher['status'] == 'completed' and publisher['conclusion'] == 'failure'
    current = api(f'repos/{REPOSITORY}/pulls/{PR}')
    assert current['state'] == 'open' and current['head']['sha'] == HEAD
    if kind == 'merge_group':
        queue = json.loads(subprocess.check_output(['gh', 'api', 'graphql', '-f', 'query=' + query], text=True))
        entry = queue['data']['repository']['pullRequest']['mergeQueueEntry']
        assert entry and entry['headCommit']['oid'] == target
    result = api(f'repos/{REPOSITORY}/statuses/{target}', dict(
        state='success', context='Local acceptance',
        description='Approved one-device enrollment; signed preflight and guard verified',
        target_url=f'https://github.com/{REPOSITORY}/actions/runs/{os.environ["GITHUB_RUN_ID"]}'))
    assert result['state'] == 'success' and result['creator']['login'] == 'github-actions[bot]'
    print(json.dumps(dict(pr=PR, approved_head=HEAD, target=target, kind=kind,
                          signed_checks=list(receipt['checks']), contract_unchanged=True,
                          ordinary_tests_passed=True, branch_protection_changed=False,
                          bootstrap_status=result['id'])))


if __name__ == '__main__':
    main()
