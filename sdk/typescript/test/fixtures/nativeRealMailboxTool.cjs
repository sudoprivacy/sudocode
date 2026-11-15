// Private stdio-to-owner transport. Tokens and text are never synthesized here.
const fs = require('node:fs');
const path = require('node:path');
const assert = require('node:assert/strict');
const root = process.env.NATIVE_MAILBOX_FIXTURE_ROOT;
assert(root && path.isAbsolute(root));
const bridge = JSON.parse(fs.readFileSync(path.join(root, 'mailbox-bridge.json')));
let chain = Promise.resolve();
require('node:readline').createInterface({ input: process.stdin }).on('line', line => {
  chain = chain.then(async () => {
    const request = JSON.parse(line); if (request.id === undefined) return;
    let result;
    if (request.method === 'initialize') result = { protocolVersion: request.params.protocolVersion, capabilities: { tools: {} },
      serverInfo: { name: 'real-mailbox-private-transport', version: '1' } };
    else if (request.method === 'tools/list') result = { tools: [{ name: 'scode_inbox', description: 'Read scode mailbox offers and acknowledge only fully handled messages.',
      inputSchema: { type: 'object', properties: { ack: { type: 'array', items: { type: 'string' } }, read: { type: 'string' },
        window: { type: 'integer', minimum: 0 }, after: { type: 'string' } }, additionalProperties: false } }] };
    else if (request.method === 'tools/call') {
      const response = await fetch(bridge.url, { method: 'POST', headers: { authorization: 'Bearer '+bridge.secret },
        body: JSON.stringify(request.params), signal: AbortSignal.timeout(15000) });
      assert.equal(response.status, 200); result = await response.json();
    } else result = {};
    fs.appendFileSync(path.join(root, 'tool-events.jsonl'), JSON.stringify({ pid: process.pid, method: request.method, params: request.params })+'\n', { mode: 0o600 });
    process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: request.id, result })+'\n');
  }).catch(error => { console.error(error); process.exitCode = 1; process.stdin.destroy(); });
});
