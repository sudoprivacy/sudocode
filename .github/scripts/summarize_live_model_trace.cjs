// Retain enough information to correlate a live failure with its gateway log.
// The artifact uses an explicit field list; request bodies and arbitrary
// headers stay out of it. Run after both successful and failed live tests.
const fs = require('node:fs');
const path = require('node:path');

const [input, output] = process.argv.slice(2);
if (!input || !output) throw new Error('Usage: summarize_live_model_trace.cjs INPUT OUTPUT');
const secret = process.env.PROXY_AUTH_TOKEN;
const clean = (value, limit = 2048) => {
  if (typeof value !== 'string') return undefined;
  return (secret ? value.split(secret).join('[REDACTED]') : value).slice(0, limit);
};
const report = { traceFiles: 0, malformedLines: 0, requests: [] };
const responses = new Map();
for (const file of [3, 2, 1, 0].map(n => n ? `${input}.${n}` : input)) {
  if (!fs.existsSync(file)) continue;
  report.traceFiles++;
  for (const line of fs.readFileSync(file, 'utf8').split(/\r?\n/)) {
    if (!line.trim()) continue;
    let event;
    try { event = JSON.parse(line); } catch { report.malformedLines++; continue; }
    const data = event.attributes ?? {};
    if (event.event === 'response_debug') {
      const ids = {};
      for (const [key, value] of Object.entries(data.headers ?? {})) {
        if (['request-id', 'x-request-id', 'x-client-request-id', 'x-oneapi-request-id'].includes(key.toLowerCase())) {
          ids[key.toLowerCase()] = clean(value, 256);
        }
      }
      responses.set(data.request_id, { status: data.status, ids });
    } else if (event.event === 'request_debug') {
      const body = data.body ?? {};
      const system = typeof body.system === 'string' ? body.system
        : (Array.isArray(body.system) ? body.system.map(block => block.text ?? '').join('\n') : '');
      const environment = system.split(/\r?\n/)
        .filter(line => /^\s*-\s*(Working directory|Platform):/.test(line))
        .slice(0, 20).map(line => clean(line));
      const assistantTools = [];
      for (const message of Array.isArray(body.messages) ? body.messages : []) {
        if (message.role !== 'assistant' || !Array.isArray(message.content)) continue;
        for (const block of message.content) {
          if (block.type !== 'tool_use') continue;
          assistantTools.push({ name: clean(block.name, 256),
            path: clean(block.input?.path ?? block.input?.file_path) });
        }
      }
      let origin;
      try { origin = new URL(data.url).origin; } catch {}
      report.requests.push({ requestId: clean(data.request_id, 256),
        sessionId: clean(event.session_id, 256), origin: clean(origin),
        model: clean(body.model, 256), messageCount: body.messages?.length ?? 0,
        environment, assistantTools });
    }
  }
}
for (const request of report.requests) request.response = responses.get(request.requestId);
fs.mkdirSync(path.dirname(output), { recursive: true });
fs.writeFileSync(output, JSON.stringify(report, null, 2) + '\n');
console.log(`Live model evidence: ${report.requests.length} requests, ${report.malformedLines} malformed lines`);
