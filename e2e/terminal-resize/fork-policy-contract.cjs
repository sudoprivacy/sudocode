// Exercise the live GitHub run/PR boundary without posting test statuses.
const fs = require('node:fs');
const path = require('node:path');

module.exports = async function forkContract(args) {
  const { context, core, root, policyPath } = args;
  if (context.payload.action !== 'completed' || context.payload.workflow_run.conclusion !== 'success')
    return;
  const publish = require('./fork-render-publish.cjs');
  const { github } = args;
  let attemptedWrites = 0;
  // Keep every read on the real API, while making an accidental test status
  // impossible even if the publisher's dry-run guard regresses.
  const readOnly = {
    rest: {
      actions: {
        getWorkflowRun: github.rest.actions.getWorkflowRun,
        getWorkflow: github.rest.actions.getWorkflow,
        listWorkflowRuns: github.rest.actions.listWorkflowRuns,
        listJobsForWorkflowRun: github.rest.actions.listJobsForWorkflowRun,
      },
      pulls: {
        list: github.rest.pulls.list,
        get: github.rest.pulls.get,
        listFiles: github.rest.pulls.listFiles,
        listReviews: github.rest.pulls.listReviews,
      },
      repos: {
        getContent: github.rest.repos.getContent,
        getCollaboratorPermissionLevel: github.rest.repos.getCollaboratorPermissionLevel,
        createCommitStatus: () => {
          attemptedWrites++;
          throw new Error('Read-only acceptance attempted to publish a status');
        },
      },
    },
    paginate: github.paginate.bind(github),
  };
  const observed = { ...args, github: readOnly, publish: false };
  const candidate = await publish(observed);
  if (candidate === undefined) {
    core.info('Stale run has no eligible publication; live acceptance applies to its replacement');
    return;
  }
  if (candidate?.status.state !== 'success')
    throw new Error('The completed real fork measurement must qualify before publishing');

  // Keep the positive case on the real SDK Context. Its repo getter must also
  // survive the copied contexts used by the rejection cases below.
  const copiedContext = { ...context, repo: context.repo };
  const staleAttempt = await publish({ ...observed, context: {
    ...copiedContext, payload: { ...context.payload, workflow_run: {
      ...context.payload.workflow_run, run_attempt: context.payload.workflow_run.run_attempt - 1,
    } },
  } });
  if (staleAttempt !== undefined) throw new Error('An old run attempt qualified for publication');

  const reject = async (label, input, expected) => {
    try {
      await publish({ ...observed, ...input });
    } catch (error) {
      if (error.message !== expected) throw error;
      core.info(`${label}: rejected against the actual GitHub run and PR`);
      return;
    }
    throw new Error(`${label}: an invalid publication unexpectedly qualified`);
  };
  // This publisher's own real run is not an eligible measurement workflow.
  await reject('wrong workflow', { context: { ...copiedContext, payload: {
    ...context.payload, workflow_run: { ...context.payload.workflow_run, id: context.runId },
  } } }, 'Expected a fork rendering pull_request run from this repository');

  const original = JSON.parse(fs.readFileSync(policyPath, 'utf8'));
  const directory = path.join(root, 'rendering-contract-negative');
  fs.mkdirSync(directory, { recursive: true });
  const invalid = path.join(directory, 'approval.json');
  fs.writeFileSync(invalid, JSON.stringify({ ...original, head: '0'.repeat(40) }));
  await reject('stale artifact head', { policyPath: invalid },
    'Measured PR base or head changed; rerun its rendering workflow');
  if (attemptedWrites !== 0) throw new Error('Acceptance attempted a status write');
  fs.writeFileSync(path.join(directory, 'receipt.json'), JSON.stringify({
    run_id: candidate.run_id, run_attempt: candidate.run_attempt,
    head: candidate.status.sha, positive: 1, rejected: 3,
    attempted_status_writes: attemptedWrites, posted_test_statuses: 0,
    context_type: context.constructor?.name,
    context_repo_is_own_property: Object.hasOwn(context, 'repo'),
  }, null, 2));
  core.info('Actual fork run qualifies; stale attempt, wrong workflow and stale artifact rejected');
};
