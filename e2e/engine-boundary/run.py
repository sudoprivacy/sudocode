#!/usr/bin/env python3
"""Exercise the boundary gate against real Git worktrees and scanner failures."""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile

GATE = Path(__file__).resolve().parents[2] / '.github/scripts/check_engine_boundary.sh'


def main():
    bash = shutil.which('bash')
    assert bash, 'bash is required'
    with tempfile.TemporaryDirectory(prefix='engine-boundary-') as directory:
        root = Path(directory)
        env = {key: value for key, value in os.environ.items() if key not in (
            'BASH_ENV', 'ENV', 'GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE',
        )}
        env.update(GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_NOSYSTEM='1')
        renderer = root / 'rust/crates/rusty-sudocode-cli/src/main.rs'
        adapter = root / 'rust/crates/engine-acp/src/lib.rs'
        backend = root / 'rust/crates/engine-core/src/lib.rs'
        for path in (renderer, adapter, backend):
            path.parent.mkdir(parents=True, exist_ok=True)
        renderer.write_text('fn main() {}\n')
        adapter.write_text('// EngineEvent adapter\n')
        backend.write_text('use runtime::RuntimeObserver;\nObserverAdapter::new();\nCompletionJournal::open();\n')
        subprocess.run(['git', 'init', '-q'], cwd=root, env=env, check=True)
        subprocess.run(['git', 'add', '.'], cwd=root, env=env, check=True)

        def expect(label, success, reason=None, overrides=None):
            result = subprocess.run([bash, str(GATE)], cwd=root,
                                    env={**env, **(overrides or {})},
                                    text=True, capture_output=True)
            assert (result.returncode == 0) == success, label + '\n' + result.stdout + result.stderr
            if reason:
                assert reason in result.stderr, label + '\n' + result.stderr
            print('PASS: ' + label)

        expect('engine internals are allowed below the seam', True)
        for forbidden in ('runtime::RuntimeObserver', 'runtime::TurnSummary'):
            renderer.write_text('use ' + forbidden + ';\n')
            expect('renderer rejects ' + forbidden, False, 'renderer bypasses')
        for forbidden in (
            'engine.run_turn(blocks)', 'engine.run_turn_with_blocks(blocks)',
            'ObserverAdapter::new()', 'SubagentRelay::new()',
            'abort_subagent(id)', 'abort_registered_agent(id)',
            'CompletionJournal::open()', 'CompletionState::stored()',
            'subagent_completion::Completions::default()',
        ):
            renderer.write_text(forbidden + ';\n')
            expect('renderer rejects ' + forbidden, False, 'renderer bypasses')
        renderer.write_text('fn main() {}\n')
        empty_tools = root / 'empty-tools'
        empty_tools.mkdir()
        expect('missing Git fails closed', False, 'git is required', {'PATH': str(empty_tools)})
        adapter.parent.parent.rename(root / 'missing-renderer')
        expect('missing renderer path fails closed', False, 'missing renderer path')
        (root / 'missing-renderer').rename(adapter.parent.parent)
        (root / '.git').rename(root / '.git-hidden')
        expect('Git search failure fails closed', False, 'git grep failed')


if __name__ == '__main__':
    main()
