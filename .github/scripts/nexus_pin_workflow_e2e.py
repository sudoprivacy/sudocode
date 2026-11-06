"""Execute the workflow's actual pin step against fresh manifest/lock inputs."""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess


def pin_step(path):
    lines = path.read_text(encoding='utf-8').splitlines()
    start = next(index for index, line in enumerate(lines) if line.strip() == 'id: pin')
    assert lines[start + 1].strip() == 'run: |'
    body = []
    for line in lines[start + 2:]:
        if line and not line.startswith('          '):
            break
        body.append(line[10:] if line else '')
    return '\n'.join(body) + '\n'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--workflow', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--bash', default=shutil.which('bash'), help='Actual Bash executable; use Git Bash on Windows')
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    bash = args.bash
    if not bash:
        raise RuntimeError('Pin workflow acceptance requires the Bash used by CI (Git Bash on Windows)')
    script = pin_step(args.workflow)
    (output / 'actual-step.sh').write_text(script, encoding='utf-8')
    source = 'https://github.com/nexi-lab/nexus-vfs'
    revision = 'a' * 40
    cases = [
        ('stable', ['v0.8.29'], [('v0.8.29', revision)], True),
        ('prerelease', ['v0.8.29-rc.5'], [('v0.8.29-rc.5', revision)], True),
        ('metadata', ['v0.8.29+build.5'], [('v0.8.29+build.5', revision)], True),
        ('wrong-prerelease', ['v0.8.29-rc.5'], [('v0.8.29-rc.4', revision)], False),
        ('stable-versus-prerelease', ['v0.8.29'], [('v0.8.29-rc.5', revision)], False),
        ('multiple-manifest-tags', ['v0.8.29-rc.4', 'v0.8.29-rc.5'], [('v0.8.29-rc.5', revision)], False),
        ('multiple-lock-tags', ['v0.8.29-rc.5'], [('v0.8.29-rc.4', revision), ('v0.8.29-rc.5', revision)], False),
        ('multiple-lock-commits', ['v0.8.29-rc.5'], [('v0.8.29-rc.5', revision), ('v0.8.29-rc.5', 'b' * 40)], False),
        ('missing-manifest-tag', [], [('v0.8.29', revision)], False),
        ('missing-lock-source', ['v0.8.29'], [], False),
    ]
    results = []
    for label, tags, locked, success in cases:
        root = output / label
        (root / 'rust').mkdir(parents=True)
        manifest = '[workspace.dependencies]\n' + ''.join(
            f'client_{index} = {{ git = "{source}", tag = "{tag}" }}\n' for index, tag in enumerate(tags))
        lock = 'version = 4\n' + ''.join(
            f'[[package]]\nname = "crate_{index}"\nversion = "1.0.0"\nsource = "git+{source}?tag={tag}#{commit}"\n'
            for index, (tag, commit) in enumerate(locked))
        (root / 'rust/Cargo.toml').write_text(manifest, encoding='utf-8')
        (root / 'rust/Cargo.lock').write_text(lock, encoding='utf-8')
        result = subprocess.run([bash, '--noprofile', '--norc', '-c', script], cwd=root,
                                env={**os.environ, 'GITHUB_OUTPUT': (root / 'github-output.txt').as_posix()},
                                capture_output=True, timeout=15)
        (root / 'process.log').write_bytes(result.stdout + result.stderr)
        published = (root / 'github-output.txt').read_text() if (root / 'github-output.txt').exists() else ''
        passed = (result.returncode == 0) == success and (
            published.strip() == f'commit={revision}' if success else not published)
        results.append({'case': label, 'expected_success': success, 'exit_code': result.returncode, 'passed': passed})
    (output / 'result.json').write_text(json.dumps({'cases': results}, indent=2) + '\n', encoding='utf-8')
    assert all(case['passed'] for case in results), results
    print(f'PASS: {len(results)} real Bash pin checks, including full prerelease tags and mismatch refusal')


if __name__ == '__main__':
    main()
