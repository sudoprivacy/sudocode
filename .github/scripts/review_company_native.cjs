// Independent read-only review. The company credential never leaves the runner.
const fs = require('node:fs');
const { spawnSync } = require('node:child_process');
const { clean, writeEvidence } = require('./live_model_evidence.cjs');
const ref = process.env.SCODE_REVIEW_REF;
const pr = process.env.SCODE_REVIEW_PR;
const channel = process.env.SCODE_REVIEW_CHANNEL;
if (!/^[0-9a-f]{40}$/.test(ref ?? '') || !/^[1-9][0-9]*$/.test(pr ?? '')) throw new Error('Exact source SHA and PR number required');
if (channel && !/^[1-9][0-9]*$/.test(channel)) throw new Error('Invalid existing channel ID');
function gh(args) {
  const result = spawnSync('gh', args, { encoding: 'utf8', maxBuffer: 8 * 1024 * 1024 });
  if (result.status !== 0) throw new Error('Read-only GitHub query failed');
  return result.stdout;
}
const metadata = JSON.parse(gh(['pr', 'view', pr, '--repo', 'sudoprivacy/sudocode', '--json', 'headRefOid,baseRefOid,title,body']));
if (metadata.headRefOid !== ref) throw new Error('PR head advanced; do not review stale source');
const diff = gh(['pr', 'diff', pr, '--repo', 'sudoprivacy/sudocode']);
const env = { ...process.env, ANTHROPIC_BASE_URL: 'https://api.sudorouter.ai' };
env.ANTHROPIC_API_KEY = process.env.PROXY_AUTH_TOKEN + (channel ? `-${channel}` : '');
delete env.ANTHROPIC_AUTH_TOKEN;
delete env.CLAUDE_CODE_OAUTH_TOKEN;
const prompt = `Review PR #${pr} at exact source ${ref}, base ${metadata.baseRefOid}.
Title: ${metadata.title}
Description:\n${metadata.body}

Review the full diff below and read relevant source and repository instructions as necessary.
Check correctness, shared ownership/SSOT, request retry/usage/stream lifecycle, preserved acceptance assertions, and integration with the base.
You have only read-only tools. Do not invoke agents, modify files, run tests, post messages, or read credentials.
Report actionable newly introduced bugs with severity and file/line references; do not invent findings.
State explicitly if no blocking issues are found and identify any coverage limitation.

DIFF:\n${diff}`;
const result = spawnSync('claude', ['-p', '--output-format', 'json', '--model', process.env.SCODE_REVIEW_MODEL,
  '--tools', 'Read,Glob,Grep', '--allowedTools', 'Read,Glob,Grep', '--max-turns', '30', '--max-budget-usd', '3'],
{ input: prompt, env, encoding: 'utf8', maxBuffer: 16 * 1024 * 1024, timeout: 15 * 60 * 1000 });
let output;
try { output = JSON.parse(result.stdout); } catch { output = { is_error: true, result: 'No structured review result' }; }
const report = { ref, base: metadata.baseRefOid, pr: Number(pr), model: process.env.SCODE_REVIEW_MODEL,
  channel, exitCode: result.status, result: JSON.parse(clean(JSON.stringify(output), 8 * 1024 * 1024)),
  stderr: clean(result.stderr, 8192), spawnError: result.error?.name };
writeEvidence(process.argv[2], report);
console.log(JSON.stringify({ ref, pr, exitCode: result.status, isError: output.is_error,
  numTurns: output.num_turns, cost: output.total_cost_usd, permissionDenials: output.permission_denials?.length }));
if (result.status !== 0 || output.is_error || !output.result || !output.total_cost_usd) process.exitCode = 1;
