const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const http = require('node:http');
const { createHost } = require('./host.cjs');
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
const document = 'ProseStart 中文 e\u0301 💻 ordinary text with **bold words**, `inline code`, and a [link](https://example.com). More ordinary prose wraps across several rows while the input stays available. ProseDone.';
const draft = 'draft界👩🏽‍💻';
const output = path.resolve('diagnostic-results');
fs.mkdirSync(output, { recursive: true });

async function run(binary, label, backend, mode, cols, round) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'scode-prose-input-'));
  const configHome = path.join(root, 'config');
  fs.mkdirSync(configHome);
  const requests = [];
  let ready = false;
  let release;
  const server = http.createServer((request, response) => {
    if (request.method === 'GET') {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ data: [{ id: 'claude-sonnet-4-6', type: 'model', display_name: 'Diagnostic Sonnet', created_at: '2026-01-01T00:00:00Z' }], has_more: false }));
      return;
    }
    if (request.url.includes('count_tokens')) {
      request.resume();
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ input_tokens: 10 }));
      return;
    }
    const chunks = [];
    request.on('data', chunk => chunks.push(chunk));
    request.on('end', async () => {
      if (!chunks.length) { response.writeHead(400); response.end(); return; }
      const body = JSON.parse(Buffer.concat(chunks).toString());
      requests.push(body);
      response.writeHead(200, { 'content-type': 'text/event-stream' });
      const event = (type, data) => response.write(`event: ${type}\ndata: ${JSON.stringify({ type, ...data })}\n\n`);
      event('message_start', { message: { id: 'diagnostic-message', type: 'message', role: 'assistant', model: 'claude-sonnet-4-6', content: [], stop_reason: null, usage: { input_tokens: 10, output_tokens: 0 } } });
      event('content_block_start', { index: 0, content_block: { type: 'text', text: '' } });
      const text = requests.length === 1 ? document : 'BufferCaptured';
      const chars = Array.from(text);
      for (let i = 0; i < chars.length; i += 12) {
        event('content_block_delta', { index: 0, delta: { type: 'text_delta', text: chars.slice(i, i + 12).join('') } });
        await sleep(20);
      }
      if (requests.length === 1) {
        ready = true;
        await new Promise(resolve => { release = resolve; });
      }
      event('content_block_stop', { index: 0 });
      event('message_delta', { delta: { stop_reason: 'end_turn', stop_sequence: null }, usage: { output_tokens: 50 } });
      event('message_stop', {});
      response.end();
    });
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  const baseUrl = `http://127.0.0.1:${server.address().port}`;
  const sample = fs.readFileSync('rust/crates/runtime/src/sudocode.sample.json', 'utf8')
    .replaceAll('https://api.anthropic.com', baseUrl)
    .replaceAll('<YOUR_ANTHROPIC_API_KEY>', 'test-diagnostic-key');
  fs.writeFileSync(path.join(configHome, 'sudocode.json'), sample);
  const name = `${label}-${backend}-${mode}-${cols}-${round}`;
  const host = createHost({ binary: path.resolve(binary), args: ['--auth', 'api-key', '--model', 'sonnet', '--permission-mode', 'read-only'], root, configHome, todos: path.join(root, 'todos.json'), logRoot: path.join(output, name), backend, cols: 80, rows: 24 });
  const result = { name, label, backend, mode, cols, round, expected: draft };
  try {
    host.resize(cols, 40);
    await host.frame(text => text.includes('❯'));
    host.child.write('Produce an ordinary paragraph\r');
    for (let i = 0; i < 400 && !ready; i++) await sleep(25);
    if (!ready) throw new Error('provider did not reach the streaming gate');
    await sleep(80);
    host.child.write(mode === 'paste' ? `\x1b[200~${draft}\x1b[201~` : draft);
    await sleep(700);
    result.before = host.viewport();
    result.visible = result.before.includes(draft);
    host.child.write('\r');
    await sleep(200);
    release();
    for (let i = 0; i < 400 && requests.length < 2; i++) await sleep(25);
    result.received = requests.length >= 2 ? requests[1].messages.filter(message => message.role === 'user').at(-1)?.content : null;
    result.bufferPreserved = JSON.stringify(result.received).includes(draft);
    result.requests = requests.length;
    console.log(JSON.stringify(result));
  } catch (error) {
    result.error = String(error.stack || error);
    console.log(JSON.stringify(result));
  } finally {
    release?.();
    await host.close();
    server.closeAllConnections();
    server.close();
    fs.writeFileSync(path.join(output, name + '.json'), JSON.stringify(result, null, 2));
  }
  return result;
}

(async () => {
  const results = [];
  for (const [label, binary] of [['main', process.argv[2]], ['candidate', process.argv[3]]])
    for (const backend of ['system', 'bundled'])
      for (const mode of ['raw', 'paste'])
        for (const cols of [120, 48])
          results.push(await run(binary, label, backend, mode, cols, 0));
  fs.writeFileSync(path.join(output, 'results.json'), JSON.stringify(results, null, 2));
  console.log(JSON.stringify({ total: results.length, visible: results.filter(row => row.visible).length, bufferPreserved: results.filter(row => row.bufferPreserved).length, errors: results.filter(row => row.error).length }));
})().catch(error => { console.error(error); process.exitCode = 1; });
