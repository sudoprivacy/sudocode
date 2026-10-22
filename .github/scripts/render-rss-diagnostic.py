"""Isolated real-terminal RSS diagnosis; no budget changes or performance verdict."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def summarize(mappings):
    groups = {}
    current = '[anonymous]'
    for line in mappings.splitlines():
        if re.match(r'^[0-9a-f]+-[0-9a-f]+\s', line):
            fields = line.split(maxsplit=5)
            current = fields[5] if len(fields) == 6 else '[anonymous]'
        elif line.startswith(('Rss:', 'Pss:', 'Private_Clean:', 'Private_Dirty:', 'Anonymous:')):
            key, value, unit = line.split()
            assert unit == 'kB'
            row = groups.setdefault(current, {})
            row[key[:-1]] = row.get(key[:-1], 0) + int(value)
    return sorted([dict(path=path, **values) for path, values in groups.items()],
                  key=lambda row: row.get('Rss', 0), reverse=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ['driver', 'base', 'candidate', 'policy', 'output']:
        parser.add_argument('--' + name, type=Path, required=True)
    args = parser.parse_args()
    assert not args.output.exists(), 'Choose a new output path; retain previous evidence'
    args.output.mkdir(parents=True)
    report = dict(scope='DIAGNOSTIC ONLY; not a performance gate or merge approval',
                  candidate_source='1769e1b2baf9f36d7168247c7d09a70929dc5e74',
                  base_sha256=digest(args.base), candidate_sha256=digest(args.candidate),
                  driver_sha256=digest(args.driver), policy_sha256=digest(args.policy), samples=[])
    for round_number in range(2):
        for case in ['long', 'cap']:
            for side in ['base', 'candidate'] if round_number == 0 else ['candidate', 'base']:
                location = args.output / f'{round_number}-{case}-{side}'
                location.mkdir()
                sample = location / 'sample.json'
                mappings = location / 'mappings.jsonl'
                binary = args.base if side == 'base' else args.candidate
                env = dict(os.environ, SCODE_TEST_BACKEND='mock', SCODE_TEST_BIN=str(binary.resolve()),
                           SUDOCODE_EXPERIMENT_PROSE_PREVIEW='0', SCODE_RENDER_CASE=case,
                           SCODE_RENDER_POLICY=str(args.policy.resolve()),
                           SCODE_RENDER_REPORT=str(sample.resolve()),
                           SCODE_TERMINAL_LOG_DIR=str(location.resolve()),
                           SCODE_RSS_DIAGNOSTIC=str(mappings.resolve()))
                with (location / 'driver.log').open('w') as log:
                    result = subprocess.run([str(args.driver.resolve()), '--ignored', '--exact',
                                             'render_performance_workload', '--nocapture'], env=env,
                                            stdout=log, stderr=subprocess.STDOUT, timeout=135)
                assert result.returncode == 0, f'{location.name}: real terminal workload failed'
                observations = [json.loads(line) for line in mappings.read_text().splitlines()]
                assert observations, 'No real process mappings captured'
                data = json.loads(sample.read_text())
                report['samples'].append(dict(label=location.name, metrics=data['metrics'],
                    resources=[dict(at=row['at'], pid=row['pid'], status=row['status'],
                                    rollup=row['rollup'], mappings=summarize(row['mappings']))
                               for row in observations]))
                (args.output / 'diagnostic.json').write_text(json.dumps(report, indent=2) + '\n')
                print(location.name, data['metrics']['rss_peak_mib'], flush=True)
    print('RSS mappings collected; no performance or merge verdict was produced')


if __name__ == '__main__':
    main()
