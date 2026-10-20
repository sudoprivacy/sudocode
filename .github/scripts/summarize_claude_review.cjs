// Retain the review outcome and tool access evidence without raw SDK messages.
const fs = require('node:fs');
const { clean, writeEvidence } = require('./live_model_evidence.cjs');
const validateReview = require('./validate_claude_review.cjs');

const [input, output] = process.argv.slice(2);
if (!output) throw new Error('Usage: summarize_claude_review.cjs INPUT OUTPUT');
const results = [];
const tools = [];
let inputError;

function tool(name, id, fields = {}) {
  if (!fields || typeof fields !== 'object') fields = {};
  return {
    name: clean(name, 256), id: clean(id, 256),
    command: clean(fields.command, 8192),
    path: clean(fields.file_path ?? fields.path, 2048),
    agentType: clean(fields.subagent_type, 256),
    model: clean(fields.model, 256),
    description: clean(fields.description, 1024),
    runInBackground: fields.run_in_background === true,
  };
}

function visit(value) {
  if (Array.isArray(value)) {
    for (const item of value) visit(item);
  } else if (value && typeof value === 'object') {
    if (value.type === 'result' && !value.parent_tool_use_id) results.push(value);
    if (value.type === 'tool_use') tools.push(tool(value.name, value.id, value.input));
    for (const nested of Object.values(value)) visit(nested);
  }
}

try {
  const text = fs.readFileSync(input, 'utf8');
  let records;
  try { records = JSON.parse(text); }
  catch { records = text.split(/\r?\n/).filter(line => line.trim()).map(line => JSON.parse(line)); }
  visit(records);
} catch (error) {
  inputError = error.name;
}

const result = results.at(-1);
const denials = (Array.isArray(result?.permission_denials) ? result.permission_denials : [])
  .map(value => tool(value.tool_name, value.tool_use_id, value.tool_input));
const text = clean(result?.result, 200000);
const subagents = Object.fromEntries(
  ['spawned', 'started_in_background', 'completed', 'failed']
    .filter(key => Number.isSafeInteger(result?.subagent_stats?.[key]))
    .map(key => [key, result.subagent_stats[key]]),
);
const report = {
  head: clean(process.env.REVIEW_HEAD_SHA, 64),
  nativeChannel: clean(process.env.REVIEW_NATIVE_CHANNEL, 64),
  inputError,
  result: result && {
    subtype: clean(result.subtype, 128), isError: result.is_error,
    turns: result.num_turns, reportedCliCostUsd: result.total_cost_usd,
    models: Object.keys(result.modelUsage ?? {}).map(value => clean(value, 256)),
    subagents,
    text,
  },
  tools,
  permissionDenials: denials,
};
writeEvidence(output, report);
report.validation = validateReview(report, process.env.REVIEW_HEAD_SHA);
writeEvidence(output, report);
if (denials.length) console.log(`::warning::Review encountered ${denials.length} tool permission denials; inspect retained tool access evidence`);
console.log(`Review evidence: ${result.num_turns} turns, ${tools.length} tool calls, ${denials.length} permission denials`);
