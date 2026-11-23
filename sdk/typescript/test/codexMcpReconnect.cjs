const assert = require('node:assert/strict');
const { spawn } = require('node:child_process');
const { createHash, randomBytes, randomUUID } = require('node:crypto');
const { once } = require('node:events');
const fs = require('node:fs');
const net = require('node:net');
const os = require('node:os');
const path = require('node:path');
const readline = require('node:readline');
const { setTimeout: delay } = require('node:timers/promises');

async function main() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scode-mcp-redial-'));
  const config = path.join(dir, 'config.json');
  const token = randomBytes(32).toString('hex'), registrationId = randomUUID();
  const ownerEvents = [], hostReplies = [], sockets = [];
  let digest, frontend, exited, diagnostics = '', release;
  const listener = net.createServer(socket => {
    sockets.push(socket);
    const number = sockets.length;
    let hello = true;
    socket.on('error', () => {});
    readline.createInterface({ input: socket }).on('line', line => {
      const msg = JSON.parse(line);
      if (hello) {
        assert.deepEqual(msg, { token, configSha256: digest, registrationId });
        hello = false;
        socket.write(JSON.stringify({ ready: true, registrationId }) + '\n');
        return;
      }
      ownerEvents.push({ number, line, ...msg });
      if (msg.id === undefined || !msg.method) return;
      const send = result => socket.write(JSON.stringify({ jsonrpc: '2.0', id: msg.id, result }) + '\n');
      if (msg.method === 'initialize') {
        const ready = () => send({ protocolVersion: '2025-06-18',
          capabilities: number === 3 ? { resources: {} } : { tools: {} }, serverInfo: { name: 'fixture', version: '1' } });
        if (number === 1 || number === 4) ready();
        else {
          release = ready;
          socket.write(JSON.stringify({ jsonrpc: '2.0', id: 'ping-' + number, method: 'ping' }) + '\n');
        }
      } else if (msg.method === 'tools/call' && msg.params.name === 'dropAfterSubmission') {
        socket.destroy();
      } else send({ tools: [] });
    });
  });
  const wait = async predicate => {
    const end = performance.now() + 7000;
    while (!predicate()) {
      assert.ok(!frontend || frontend.exitCode === null, diagnostics);
      assert.ok(performance.now() < end, 'Timed out waiting for MCP redial fixture: ' + diagnostics);
      await delay(10);
    }
  };
  try {
    listener.listen(0, '127.0.0.1'); await once(listener, 'listening');
    fs.writeFileSync(config, JSON.stringify({ version: 1, mode: 'codex-autojoin', native: { codexArgv: [process.execPath] },
      stateDirectory: dir, auditLog: path.join(dir, 'audit'), peers: [], enrollmentArgv: [process.execPath],
      endpoint: { port: listener.address().port, token } }), { mode: 0o600 });
    digest = createHash('sha256').update(fs.readFileSync(config)).digest('hex');
    frontend = spawn(process.execPath, [path.resolve(__dirname, '../dist/server.cjs'), '--codex', '--config', config,
      '--registration-id', registrationId], { stdio: 'pipe' });
    exited = once(frontend, 'close');
    frontend.stderr.on('data', b => { diagnostics = (diagnostics + b).slice(-1000); });
    readline.createInterface({ input: frontend.stdout }).on('line', line => hostReplies.push(JSON.parse(line)));
    const send = msg => frontend.stdin.write(JSON.stringify({ jsonrpc: '2.0', ...msg }) + '\n');
    const response = async id => {
      await wait(() => hostReplies.some(msg => msg.id === id));
      return hostReplies.find(msg => msg.id === id);
    };
    const releaseAfterInput = async number => {
      const id = 'ping-' + number;
      await response(id);
      send({ id, result: {} });
      await wait(() => ownerEvents.some(msg => msg.number === number && msg.id === id && !msg.method));
      release(); release = undefined;
    };
    send({ id: 1, method: 'initialize', params: { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'frontend', version: '1' } } });
    await response(1);
    send({ method: 'notifications/initialized' });
    send({ id: 2, method: 'tools/call', params: { name: 'dropAfterSubmission' } });
    const lost = await response(2);
    assert.equal(lost.result.isError, true);
    assert.equal(JSON.parse(lost.result.content[0].text).outcome, 'unknown');
    await wait(() => !!release);
    send({ id: 3, method: 'tools/call', params: { name: 'cancelledBeforeSubmission' } });
    send({ method: 'notifications/cancelled', params: { requestId: 3 } });
    const original = '{"jsonrpc":"2.0", "id":4,"method":"tools/list","params":{"_meta":{"host":"client 零"}}}';
    // Fragment a multibyte character and preserve the original wire bytes.
    const bytes = Buffer.from(original + '\r\n');
    const split = bytes.indexOf(Buffer.from('零')) + 1;
    frontend.stdin.write(bytes.subarray(0, split)); frontend.stdin.write(bytes.subarray(split));
    await releaseAfterInput(2);
    assert.ok((await response(4)).result);
    assert.equal(ownerEvents.find(msg => msg.id === 4).line, original);
    assert.equal(ownerEvents.filter(msg => msg.id === 4)[0].number, 2);
    assert.ok(!ownerEvents.some(msg => msg.id === 3 || msg.method === 'notifications/cancelled'));
    assert.deepEqual(ownerEvents.filter(msg => msg.number === 2 && msg.method).map(msg => msg.method),
      ['initialize', 'notifications/initialized', 'tools/list']);

    sockets[1].destroy();
    await wait(() => !!release);
    send({ id: 5, method: 'tools/call', params: { name: 'incompatibleOwner' } });
    await releaseAfterInput(3);
    const refused = await response(5);
    assert.equal(JSON.parse(refused.result.content[0].text).outcome, 'not_submitted');
    assert.ok(!ownerEvents.some(msg => msg.id === 5));
    send({ id: 6, method: 'tools/list', params: {} });
    assert.ok((await response(6)).result);
    assert.equal(hostReplies.filter(msg => msg.id === 1).length, 1);
    assert.equal(ownerEvents.filter(msg => msg.method === 'tools/call').length, 1);
    frontend.stdin.end();
    assert.deepEqual(await exited, [0, null]);
    console.log('PASS stdio owner redial: handshake replay, original bytes, cancellation, ambiguous submission and incompatible capabilities');
  } finally {
    if (frontend && frontend.exitCode === null && frontend.signalCode === null) { frontend.kill(); await exited; }
    for (const socket of sockets) socket.destroy();
    await new Promise(resolve => listener.close(resolve));
    fs.rmSync(dir, { recursive: true, force: true });
  }
}
main().catch(error => { console.error(error); process.exitCode = 1; });
