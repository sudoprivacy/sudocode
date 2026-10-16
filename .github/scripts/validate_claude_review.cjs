const fs = require('node:fs');

const MINIMUM_AGENTS = 4;

/** Validate the retained execution record, including real completed fan-out. */
function validateReview(report, expectedHead) {
  const result = report.result;
  const agents = result?.subagents;
  const tools = Array.isArray(report.tools) ? report.tools : [];
  const agentTools = tools.filter(tool => ['Agent', 'Task'].includes(tool.name));
  const agentIds = new Set(agentTools.map(tool => tool.id).filter(id => typeof id === 'string' && id));
  if (!/^[0-9a-f]{40}$/.test(expectedHead ?? '') || report.head !== expectedHead) {
    throw new Error('Review evidence does not match the requested immutable head');
  }
  if (report.inputError || !result || result.subtype !== 'success' || result.isError !== false
      || !Number.isSafeInteger(result.turns) || result.turns <= 1 || !result.text?.trim()) {
    throw new Error('Review has no successful multi-turn execution and final result; inspect retained evidence');
  }
  if (!agents || !['spawned', 'completed', 'failed', 'started_in_background']
    .every(key => Number.isSafeInteger(agents[key]) && agents[key] >= 0)) {
    throw new Error('Review has no complete agent execution accounting');
  }
  if (agents.started_in_background || agentTools.some(tool => tool.runInBackground)) {
    throw new Error('Review launched background agents; require synchronous completed reviews');
  }
  if (agents.spawned < MINIMUM_AGENTS || agents.completed !== agents.spawned || agents.failed
      || agentIds.size < MINIMUM_AGENTS) {
    throw new Error(`Review did not complete all ${MINIMUM_AGENTS} independent agents; an eligibility skip is not a passing review`);
  }
  return {
    head: expectedHead, fullReview: true, minimumAgents: MINIMUM_AGENTS,
    completedAgents: agents.completed, uniqueAgentCalls: agentIds.size,
  };
}

module.exports = validateReview;
if (require.main === module) {
  const file = process.argv[2];
  if (!file) throw new Error('Usage: validate_claude_review.cjs REPORT');
  const validation = validateReview(JSON.parse(fs.readFileSync(file, 'utf8')), process.env.REVIEW_HEAD_SHA);
  console.log(JSON.stringify(validation));
}
