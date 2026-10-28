// Trusted publisher: inspect GitHub metadata and JSON data; never execute a
// candidate checkout or an artifact. The measurement workflow has read access.
const fs = require('node:fs');
const path = require('node:path');

module.exports = async function publishFork({ github, context, core, root, policyPath, publish = true }) {
  const repository = `${context.repo.owner}/${context.repo.repo}`;
  const eventRun = context.payload.workflow_run;
  if (!Number.isSafeInteger(eventRun?.id)) throw new Error('A workflow_run ID is required');
  const run = (await github.rest.actions.getWorkflowRun({
    ...context.repo, run_id: eventRun.id,
  })).data;
  const workflow = (await github.rest.actions.getWorkflow({
    ...context.repo, workflow_id: 'render-performance.yml',
  })).data;
  if (run.repository.full_name !== repository || run.workflow_id !== workflow.id
    || run.path.split('@')[0] !== '.github/workflows/render-performance.yml'
    || run.event !== 'pull_request' || run.head_repository.full_name === repository)
    throw new Error('Expected a fork rendering pull_request run from this repository');
  const completed = context.payload.action === 'completed';
  if (run.run_attempt !== eventRun.run_attempt || (completed && run.status !== 'completed')
    || (!completed && run.status === 'completed')) {
    core.info('Run advanced; only its current lifecycle event may publish');
    return;
  }
  // Fork workflow_run payloads can have an empty pull_requests array. Bind the
  // run through its actual repository, branch and immutable commit instead.
  const prs = await github.paginate(github.rest.pulls.list, {
    ...context.repo, state: 'open', base: 'main',
    head: `${run.head_repository.owner.login}:${run.head_branch}`, per_page: 100,
  });
  const matches = prs.filter(pr => pr.head.sha === run.head_sha
    && pr.head.repo?.id === run.head_repository.id && pr.head.ref === run.head_branch);
  if (!matches.length) {
    core.info('PR closed or advanced; do not publish a stale rendering gate');
    return;
  }
  if (matches.length !== 1) throw new Error('Rendering run must identify exactly one open PR');
  const pr = (await github.rest.pulls.get({ ...context.repo, pull_number: matches[0].number })).data;
  const runs = await github.paginate(github.rest.actions.listWorkflowRuns, {
    ...context.repo, workflow_id: workflow.id, event: 'pull_request',
    head_sha: run.head_sha, per_page: 100,
  });
  const latest = runs.filter(candidate => candidate.head_repository.id === run.head_repository.id
    && candidate.head_branch === run.head_branch).sort((a, b) => b.id - a.id)[0];
  if (latest?.id !== run.id) {
    core.info('A newer rendering run exists; preserve its gate');
    return;
  }
  if (!completed) {
    const status = { ...context.repo, sha: run.head_sha, context: 'render performance',
      state: 'pending', description: 'Waiting for policy, A/A calibration and A/B measurements',
      target_url: run.html_url };
    if (publish) await github.rest.repos.createCommitStatus(status);
    return { pull_number: pr.number, run_id: run.id, run_attempt: run.run_attempt, status };
  }
  const jobs = await github.paginate(github.rest.actions.listJobsForWorkflowRun, {
    ...context.repo, run_id: run.id, filter: 'latest', per_page: 100,
  });
  const jobPassed = name => jobs.some(job => job.name === name && job.conclusion === 'success');
  let success = run.conclusion === 'success'
    && jobPassed('Rendering policy live PR contract') && jobPassed('Rendering benchmark policy');
  let reason = 'Policy, calibration or performance failed; inspect the rendering run';
  if (success) {
    // Read only the small, fixed-name metadata file. Uploaded paths and strings
    // are never imported, interpolated into a command or used to select code.
    if (fs.statSync(policyPath).size > 128 * 1024) throw new Error('Rendering metadata is too large');
    const selected = JSON.parse(fs.readFileSync(policyPath, 'utf8'));
    if (selected.head !== run.head_sha || selected.head !== pr.head.sha || selected.base !== pr.base.sha)
      throw new Error('Measured PR base or head changed; rerun its rendering workflow');
    const outputs = {};
    const approval = await require(path.join(root, 'e2e/terminal-resize/policy-review.cjs'))({
      github, context: { ...context, eventName: 'pull_request', sha: run.head_sha,
        payload: { pull_request: pr } },
      core: { ...core, setOutput: (name, value) => { outputs[name] = value; } },
      root, output: path.join(root, 'final-rendering-approval'),
    });
    if (approval.head !== selected.head || approval.base !== selected.base)
      throw new Error('Independent policy verification disagrees with the measured revision');
    // The trusted policy must cover edits to either workflow and this publisher,
    // so candidate-controlled workflow changes still require exact-head review.
    success = outputs.is_needed === 'true' ? jobPassed('Release rendering A/A and A/B')
      : outputs.is_needed === 'false' && jobs.some(job => job.name === 'Release rendering A/A and A/B'
        && job.conclusion === 'skipped');
    if (success) reason = outputs.is_needed === 'false'
      ? 'No runtime or benchmark changes' : 'Release A/A calibration and A/B budgets passed';
  }
  const current = (await github.rest.pulls.get({ ...context.repo, pull_number: pr.number })).data;
  if (current.head.sha !== run.head_sha || current.base.sha !== pr.base.sha)
    throw new Error('PR advanced before publishing; retain its unqualified gate');
  const finalRun = (await github.rest.actions.getWorkflowRun({
    ...context.repo, run_id: run.id,
  })).data;
  if (finalRun.run_attempt !== run.run_attempt || finalRun.status !== 'completed')
    throw new Error('Rendering run retried before publishing; wait for its current attempt');
  const status = { ...context.repo, sha: run.head_sha, context: 'render performance',
    state: success ? 'success' : 'failure', description: reason, target_url: run.html_url };
  if (publish) {
    await github.rest.repos.createCommitStatus(status);
    if (!success) core.setFailed(reason);
  }
  return { pull_number: pr.number, run_id: run.id, run_attempt: run.run_attempt, status };
};
