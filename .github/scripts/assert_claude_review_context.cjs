const { execFileSync } = require('node:child_process');

/** Check the live PR and checkout at both boundaries of one review. */
module.exports = async function assertCurrentReview({ github, context }) {
  const requested = context.payload.pull_request;
  const head = requested?.head?.sha;
  if (!requested?.number || !/^[0-9a-f]{40}$/.test(head ?? '')) {
    throw new Error('Review requires a pull-request number and immutable head');
  }
  const { data: current } = await github.rest.pulls.get({
    ...context.repo, pull_number: requested.number,
  });
  if (current.state !== 'open' || current.draft || current.head.sha !== head) {
    throw new Error('PR is no longer ready at this commit; review its current head');
  }
  const checkout = execFileSync('git', ['rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
  if (checkout !== head) throw new Error('Review checkout does not match the requested commit');
  return `Current ready review: ${context.repo.owner}/${context.repo.repo}#${requested.number} @ ${head}`;
};
