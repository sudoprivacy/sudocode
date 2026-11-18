// Deterministic presentation probe, executed by the real native MCP client.
// No ACP/ledger: this qualifies native presentation only, never real ACK custody.
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const root = process.env.NATIVE_MAILBOX_FIXTURE_ROOT;
assert(root && path.isAbsolute(root));
const fixture = JSON.parse(fs.readFileSync(path.join(root, 'tool-fixture.json')));
const record = value => fs.appendFileSync(path.join(root, 'tool-events.jsonl'),
  JSON.stringify({ pid: process.pid, threadId: process.env.CODEX_THREAD_ID, ...value })+'\n', { mode: 0o600 });
require('node:readline').createInterface({ input: process.stdin }).on('line', line => {
  const request = JSON.parse(line);
  if (request.id === undefined) return;
  let result;
  if (request.method === 'initialize') {
    record({ method: request.method, params: request.params });
    result = { protocolVersion: request.params.protocolVersion, capabilities: { tools: {} },
      serverInfo: { name: 'mailbox-presentation-fixture', version: process.env.NATIVE_MAILBOX_FIXTURE_GENERATION || '1' } };
  } else if (request.method === 'tools/list') {
    result = { tools: [{ name: 'scode_inbox', description: 'Read the next isolated mailbox fixture window.',
      inputSchema: { type: 'object', properties: { read: { type: 'string' } }, required: ['read'], additionalProperties: false } }] };
  } else if (request.method === 'tools/call') {
    record({ method: request.method, params: request.params });
    assert.equal(request.params.name, 'scode_inbox');
    assert.deepEqual(request.params.arguments, { read: fixture.cursor });
    result = { content: [{ type: 'text', text: fixture.text }], isError: false };
    assert(Buffer.byteLength(JSON.stringify(result)) <= fixture.encodedResultBytes);
  } else result = {};
  process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id, result })+'\n');
});
