// Read-only integration acceptance against the live PR, not a mocked API.
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const review = require('./policy-review.cjs');

module.exports = async function contract({ github, context, core, root }) {
  const number = context.payload.pull_request?.number || context.payload.inputs?.pull_request;
  if (!number) {
    core.info('No PR selected; stale-PR acceptance applies to PR rendering gates');
    return;
  }
  if (!/^[1-9][0-9]*$/.test(String(number))) throw new Error('Invalid PR number');
  const pr = (await github.rest.pulls.get({ ...context.repo, pull_number: Number(number) })).data;
  const stale = '0'.repeat(40);
  const output = fs.mkdtempSync(path.join(os.tmpdir(), 'scode-render-policy-'));
  const reject = async (label, candidate, expected) => {
    try {
      await review({ github, context: candidate, core, root, output });
    } catch (error) {
      if (error.message !== expected) throw error;
      core.info(`${label}: rejected stale commit against live PR #${pr.number}`);
      return;
    }
    throw new Error(`${label}: stale commit unexpectedly passed policy validation`);
  };
  try {
    await reject('manual dispatch', {
      ...context, repo: context.repo, eventName: 'workflow_dispatch', sha: stale,
      payload: { inputs: { pull_request: String(pr.number) } },
    }, 'Dispatch the open main PR from its current same-repository head branch');
    await reject('PR event', {
      ...context, repo: context.repo, eventName: 'pull_request',
      payload: { pull_request: { ...pr, head: { ...pr.head, sha: stale } } },
    }, 'PR advanced; run the current commit');
    const files = await github.paginate(github.rest.pulls.listFiles, {
      ...context.repo, pull_number: pr.number, per_page: 100,
    });
    const ordinaryTest = name => /^rust\/crates\/[^/]+\/tests\//.test(name)
      && !name.startsWith('rust/crates/rusty-sudocode-cli/tests/common/')
      && name !== 'rust/crates/rusty-sudocode-cli/tests/pty_render_performance.rs';
    if (files.length && files.every(file => ordinaryTest(file.filename)
      && (!file.previous_filename || ordinaryTest(file.previous_filename)))) {
      const outputs = {};
      await review({ github, context: {
        ...context, eventName: 'pull_request', payload: { pull_request: pr },
      }, core: { ...core, setOutput: (key, value) => { outputs[key] = value; } }, root, output });
      if (outputs.is_needed !== 'false') throw new Error('Test-only PR requested a release performance rebuild');
      core.info(`Live test-only PR #${pr.number}: release performance rebuild skipped`);
    }
  } finally {
    fs.rmSync(output, { recursive: true, force: true });
  }
};
