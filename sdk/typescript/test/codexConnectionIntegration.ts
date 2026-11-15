import * as assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { once } from 'node:events';
import { WebSocketServer } from 'ws';
import { connectRecoveringCodex, CodexCleanupUnknownError as RecoveryCleanupUnknownError } from '../src/codexConnection';
const root = path.resolve(__dirname, '../..');
const { connectCodex, connectCodexCallback, createCodexCallback, CodexRequestError, CodexCleanupUnknownError } = require(process.argv[2] ?? path.join(root, 'dist/index.cjs')) as typeof import('../src/index');

async function main(): Promise<void> {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'scode-codex-connection-'));
  const server = new WebSocketServer({ host: '127.0.0.1', port: 0 });
  await once(server, 'listening');
  const address = server.address();
  assert.ok(address && typeof address !== 'string');
  const proxyEnv: NodeJS.ProcessEnv = { ...process.env, SCODE_NATIVE_ENV_SENTINEL: 'owned-proxy' };
  delete proxyEnv.CODEX_THREAD_ID;
  const config = { codexArgv: [process.execPath, path.join(root, 'test/fixtures/codex-stdio-proxy.mjs'), String(address.port), 'normal'],
    socket: 'fixture-native.sock', env: proxyEnv as Record<string, string> };
  const calls: Array<{ method: string; params?: Record<string, unknown> }> = [];
  const clientReplies: unknown[] = [];
  const original = process.env.CODEX_THREAD_ID ?? 'fixture-original';
  let state = 'idle';
  let connections = 0, dropNextDelivery = false;
  let durableQueueVersion: unknown = 1;
  const accepted = new Map<string, any>();
  server.on('connection', socket => {
    connections++;
    socket.on('message', bytes => {
      const call = JSON.parse(bytes.toString());
      if (typeof call.method !== 'string') { clientReplies.push(call); return; }
      calls.push(call);
      if (call.method === 'initialized') {
        socket.send(JSON.stringify({ jsonrpc: '2.0', method: 'thread/started', params: { thread: { id: original } } }));
        return;
      }
      if (call.method === 'fixture/drop') { socket.close(); return; }
      if (call.method === 'fixture/rpc-error') {
        socket.send(JSON.stringify({ jsonrpc: '2.0', id: call.id,
          error: { code: -32000, message: 'failed after admission', data: { mayHaveExecuted: true } } }));
        return;
      }
      if (call.method === 'fixture/observer-before-reply') {
        socket.send(JSON.stringify({ jsonrpc: '2.0', method: call.params.asynchronous ? 'fixture/async-throw' : 'fixture/sync-throw' }));
        setTimeout(() => {
          if (socket.readyState === 1) socket.send(JSON.stringify({ jsonrpc: '2.0', id: call.id, result: { alive: true } }));
        }, 40);
        return;
      }
      if (call.method === 'fixture/server-request-collision') {
        socket.send(JSON.stringify({ jsonrpc: '2.0', id: call.id,
          method: 'account/chatgptAuthTokens/refresh', params: { reason: 'unauthorized' } }));
        setTimeout(() => {
          if (socket.readyState === 1) socket.send(JSON.stringify({ jsonrpc: '2.0', id: call.id,
            result: { source: 'actual response' } }));
        }, 40);
        return;
      }
      if (call.method === 'thread/queue/addOnce') {
        const key = call.params.clientUserMessageId;
        const existing = accepted.get(key);
        if (existing) assert.deepEqual(existing.input, call.params.input);
        else accepted.set(key, { id: key, clientUserMessageId: key, input: call.params.input });
        if (dropNextDelivery) { dropNextDelivery = false; socket.terminate(); return; }
        socket.send(JSON.stringify({ jsonrpc: '2.0', id: call.id, result: {queuedSubmission: accepted.get(key)} }));
        return;
      }
      const result = call.method === 'initialize' ? { userAgent: 'native-protocol-fixture/0.163.0',
        ...(durableQueueVersion === undefined ? {} : {serverCapabilities:{durableQueueAddOnce:durableQueueVersion}}) }
        : call.method === 'thread/loaded/list' ? { data: [original, 'another'], nextCursor: null }
          : call.method === 'thread/read' ? { thread: { id: call.params.threadId, cwd: directory, status: { type: state } } }
            : call.method === 'thread/turns/list' ? { data: [{ id: 'running', status: 'inProgress' }] }
              : call.method === 'turn/steer' ? { turnId: 'running' } : { turn: { id: 'fresh' } };
      socket.send(JSON.stringify({ jsonrpc: '2.0', id: call.id, result }));
    });
  });
  const waitFor = async (predicate: () => boolean) => {
    const deadline = Date.now() + 3000;
    while (!predicate() && Date.now() < deadline) await new Promise(resolve => setTimeout(resolve, 10));
    assert.ok(predicate(), 'native process integration did not reach the expected state');
  };
  try {
    await assert.rejects(connectCodex({ codexArgv: ['bad\0argv'] }), /configuration/);
    await assert.rejects(connectCodex({ ...config, env: { 'bad\0name': 'value' } }), /configuration/);
    await assert.rejects(connectCodex({ ...config, env: { VALUE: 1 } as unknown as Record<string, string> }), /configuration/);
    if (process.env.CODEX_THREAD_ID) {
      await assert.rejects(connectCodexCallback({ ...config, threadId: `${original}-different`, peers: [],
        log: path.join(directory, 'wrong-owner.jsonl') }, 'existing-acp'), /owning CODEX_THREAD_ID/);
      assert.equal(fs.existsSync(path.join(directory, 'wrong-owner.jsonl')), false);
    }
    assert.equal(connections, 0);
    const events: string[] = [];
    const observerErrors: unknown[] = [];
    const native = await connectCodex({ ...config, onNotificationError(error) {
      observerErrors.push(error);
      if (observerErrors.length === 1) throw new Error('diagnostic observer also failed');
      return Promise.reject(new Error('asynchronous diagnostic observer also failed'));
    } }, event => {
      events.push(event.method);
      if (event.method === 'fixture/sync-throw') throw new Error('synchronous observer failed');
      if (event.method === 'fixture/async-throw') return Promise.reject(new Error('asynchronous observer failed'));
    });
    try {
      assert.deepEqual(native.initialized, { userAgent: 'native-protocol-fixture/0.163.0', serverCapabilities:{durableQueueAddOnce:1} });
      assert.deepEqual(await native.request('thread/loaded/list', {}), { data: [original, 'another'], nextCursor: null });
      await waitFor(() => events.includes('thread/started'));
      assert.equal(connections, 1, 'inventory and notifications share the same official-proxy connection');
      assert.deepEqual(await native.request('fixture/server-request-collision', {}), { source: 'actual response' },
        'a broadcast server request with the same numeric ID must not consume the pending response');
      assert.equal(clientReplies.length, 0, 'inspection must not race the frontend to answer an auth broadcast');
      assert.ok(!events.includes('account/chatgptAuthTokens/refresh'), 'server requests are not notifications');
      for (const asynchronous of [false, true]) {
        assert.deepEqual(await native.request('fixture/observer-before-reply', { asynchronous }), { alive: true },
          'neither a notification observer nor its diagnostic sink may reject pending requests');
      }
      assert.equal(observerErrors.length, 2);
      assert.equal(native.lastNotificationError, observerErrors[1]);
      assert.match(String(native.lastNotificationError), /asynchronous observer failed/);
      const circular: Record<string, unknown> = {};
      circular.self = circular;
      await assert.rejects(native.request('fixture/encode', circular), error =>
        error instanceof CodexRequestError && error.outcome === 'not_sent');
      assert.equal(calls.filter(call => call.method === 'fixture/encode').length, 0);
      await assert.rejects(native.request('fixture/rpc-error', {}), error => {
        assert.ok(error instanceof CodexRequestError);
        assert.equal(error.outcome, 'unknown', 'an RPC error alone does not prove no side effect');
        assert.deepEqual(error.rpcError, { code: -32000, message: 'failed after admission', data: { mayHaveExecuted: true } });
        return true;
      });
      await assert.rejects(native.request('fixture/drop', {}), error =>
        error instanceof CodexRequestError && error.outcome === 'unknown' && /closed|exited/.test(error.message));
      assert.equal(calls.filter(call => call.method === 'fixture/drop').length, 1, 'response loss is never retried');
    } finally { await native.close(); await native.close(); }
    await assert.rejects(native.request('thread/loaded/list', {}), error =>
      error instanceof CodexRequestError && error.outcome === 'not_sent');

    const stubborn = await connectCodex({ ...config, closeTimeoutMs: 600,
      codexArgv: [...config.codexArgv.slice(0, -1), 'ignore-close'] });
    let exited = false;
    void stubborn.exited.then(() => { exited = true; });
    const closingAt = Date.now();
    const closing = stubborn.close();
    assert.equal(stubborn.close(), closing, 'concurrent close calls share the actual child-exit barrier');
    await closing;
    assert.equal(exited, true, 'close cannot report success before its spawned proxy exits');
    assert.ok(Date.now() - closingAt < 2000, 'an uncooperative owned proxy must stop within the configured bounded phases');
    await assert.rejects(connectCodex({ codexArgv: [path.join(directory, 'missing-native-binary')] }), /ENOENT|exited/);
    await assert.rejects(connectCodex({ ...config,
      codexArgv: [...config.codexArgv.slice(0, -1), 'http-refusal'] }), /Unexpected server response: 503/);

    for (const version of [undefined, 0, '1', 2]) {
      durableQueueVersion = version;
      const before = calls.length;
      await assert.rejects(connectCodex({...config, requireDurableQueue:true}), /did not negotiate durable/);
      assert.deepEqual(calls.slice(before).map(call=>call.method), ['initialize'],
        'a version label cannot replace actual capability; no application request is sent');
    }
    durableQueueVersion = 1;
    const qualified = await connectRecoveringCodex({...config, requireDurableQueue:true});
    try {
      await qualified.request('thread/loaded/list', {});
      await assert.rejects(qualified.request('fixture/drop', {}));
      durableQueueVersion = undefined;
      const before = calls.length;
      await assert.rejects(qualified.request('thread/queue/addOnce', {threadId:original}), /did not negotiate durable/);
      assert.deepEqual(calls.slice(before).map(call=>call.method), ['initialize'],
        'same-address reconnect to an older server refuses before a write; no legacy fallback');
    } finally { await qualified.close(); durableQueueVersion = 1; }

    // POSIX inherited pipe lifetime is the failure trigger here. Windows
    // cleanup ownership has its separate real bootstrap-failure regression.
    if (process.platform !== 'win32') {
    const recoveryJournal = path.join(directory, 'recovery-cleanup.jsonl');
    const recovering = await connectRecoveringCodex({ ...config, closeTimeoutMs: 300,
      codexArgv: [...config.codexArgv.slice(0, -1), 'fail-reconnect-close'],
      env: { ...config.env, SCODE_FIXTURE_RECONNECT_JOURNAL: recoveryJournal } });
    let recoveryError: unknown;
    try {
      await assert.rejects(recovering.request('fixture/drop', {}));
      await assert.rejects(recovering.request('thread/loaded/list', {}), error => {
        recoveryError = error;
        return error instanceof RecoveryCleanupUnknownError;
      });
      await assert.rejects(recovering.close(), error => error === recoveryError);
      await waitFor(() => fs.existsSync(recoveryJournal + '.drained'));
      // Later pipe draining does not erase the retained unconfirmed cleanup.
      await assert.rejects(recovering.close(), error => error === recoveryError);
      await assert.rejects(recovering.request('thread/loaded/list', {}), /closed/);
      const spawned = fs.readFileSync(recoveryJournal, 'utf8').trim().split('\n').map(line => JSON.parse(line));
      assert.equal(spawned.filter(row => row.phase === 'proxy').length, 2, 'failed replacement is never retried');
      assert.equal(spawned.filter(row => row.phase === 'stderr-holder').length, 1);
    } finally {
      await recovering.close().catch(error => { if (error !== recoveryError) throw error; });
      if (fs.existsSync(recoveryJournal) && fs.readFileSync(recoveryJournal, 'utf8').includes('stderr-holder')) {
        await waitFor(() => fs.existsSync(recoveryJournal + '.drained'));
      }
    }
    }

    const message = { sessionId: 'existing-acp', text: '<mailbox-message from="allowed-peer">peer data 零 🧪</mailbox-message>',
      body: 'peer data 零 🧪', sudocode: { a2a: { from: 'allowed-peer' } } };
    for (const mode of ['idle', 'active']) {
      state = mode;
      const log = path.join(directory, `${mode}.jsonl`);
      const cfg = { ...config, threadId: original, peers: ['allowed-peer'], log };
      const stable = { ...message, deliveryId: (mode === 'idle' ? 'a' : 'b').repeat(64) };
      dropNextDelivery = true;
      let callback = await connectCodexCallback(cfg, 'existing-acp');
      try {
        callback.receive({ ...stable, sessionId: 'different-acp' });
        callback.receive({ ...stable, sudocode: { a2a: { from: 'unlisted-peer' } } });
        callback.receive(stable);
        await waitFor(() => fs.readFileSync(log, 'utf8').includes('"phase":"pending"'));
        // Restart after commit/response loss: reload the durable inbox and
        // retry exactly the same ID against the same server.
        await callback.close();
        callback = await connectCodexCallback(cfg, 'existing-acp');
        await waitFor(() => fs.readFileSync(log, 'utf8').includes('"phase":"queued"'));
        const count = calls.filter(call => call.method === 'thread/queue/addOnce').length;
        callback.receive(stable);
        await new Promise(resolve => setTimeout(resolve, 50));
        assert.equal(calls.filter(call => call.method === 'thread/queue/addOnce').length, count,
          'upstream replay after durable acceptance must not submit again');
      } finally { await callback.close(); }
    }
    assert.equal(accepted.size, 2, 'one logical queue entry per distinct delivery, despite response loss and restart');
    // Reconnection without restarting the callback owner.
    const reconnectLog = path.join(directory, 'reconnect.jsonl');
    const reconnect = await connectCodexCallback({ ...config, threadId: original, peers: ['allowed-peer'], log: reconnectLog }, 'existing-acp');
    try {
      dropNextDelivery = true;
      reconnect.receive({ ...message, deliveryId: 'c'.repeat(64) });
      await waitFor(() => fs.readFileSync(reconnectLog, 'utf8').includes('"phase":"queued"'));
      assert.equal(accepted.size, 3);
    } finally { await reconnect.close(); }
    const orderedLog = path.join(directory, 'ordered.jsonl');
    const orderedConfig = { ...config, threadId: original, peers: ['allowed-peer'], log: orderedLog };
    let ordered = await connectCodexCallback(orderedConfig, 'existing-acp');
    const originalClock = Date.now;
    try {
      Date.now = () => 20000;
      ordered.receive({ ...message, deliveryId: '6'.repeat(64), text: 'FIRST_IN_ORDER', body: 'FIRST_IN_ORDER' });
      Date.now = () => 10000; // A wall-clock backstep cannot reverse durable FIFO.
      ordered.receive({ ...message, deliveryId: '7'.repeat(64), text: 'SECOND_IN_ORDER', body: 'SECOND_IN_ORDER' });
    } finally { Date.now = originalClock; await ordered.close(); }
    const beforeOrdered = calls.length;
    ordered = await connectCodexCallback(orderedConfig, 'existing-acp');
    try {
      await waitFor(() => fs.readFileSync(orderedLog, 'utf8').split('"phase":"queued"').length === 3);
      const delivered = calls.slice(beforeOrdered).filter(call => call.method === 'thread/queue/addOnce');
      assert.equal(delivered.length, 2);
      assert.ok(JSON.stringify(delivered[0].params).includes('FIRST_IN_ORDER'));
      assert.ok(JSON.stringify(delivered[1].params).includes('SECOND_IN_ORDER'));
    } finally { await ordered.close(); }
    // Disk/capacity refusal must be synchronous so ACP cannot ACK missing
    // custody. These calls use the same real proxy and private on-disk inbox.
    const boundedLog = path.join(directory, 'bounded.jsonl');
    const bounded = await connectCodexCallback({ ...config, threadId: original,
      peers: ['allowed-peer'], log: boundedLog }, 'existing-acp');
    const beforeBounded = calls.filter(call => call.method === 'thread/queue/addOnce').length;
    const retained = { ...message, deliveryId: 'd'.repeat(64) };
    bounded.receive(retained);
    assert.throws(() => bounded.receive({ ...retained, body: 'changed' }), /different bytes/);
    assert.throws(() => bounded.receive({ ...message, text: 'x'.repeat(256 * 1024) }), /limit/);
    const expanded = { ...message, text: '"'.repeat(70000), body: '' };
    assert.ok(Buffer.byteLength(JSON.stringify(expanded)) < 256 * 1024);
    assert.throws(() => bounded.receive(expanded), /Encoded peer delivery.*limit/);
    for (let n = 1; n < 64; n++) bounded.receive({ ...message, deliveryId: n.toString(16).padStart(64, '0') });
    assert.throws(() => bounded.receive({ ...message, deliveryId: 'e'.repeat(64) }), /inbox full/);
    await bounded.close();
    assert.throws(() => bounded.receive(retained), /closed/);
    assert.equal(calls.filter(call => call.method === 'thread/queue/addOnce').length, beforeBounded);
    const recordFile = path.join(boundedLog + '.inbox', fs.readdirSync(boundedLog + '.inbox').find(name => name.endsWith('.json'))!);
    const record = JSON.parse(fs.readFileSync(recordFile, 'utf8'));
    record.threadId = 'different-owner'; fs.writeFileSync(recordFile, JSON.stringify(record));
    await assert.rejects(connectCodexCallback({ ...config, threadId: original, peers: ['allowed-peer'],
      log: boundedLog }, 'existing-acp'), /identity or content changed/);

    const failingLog = path.join(directory, 'storage-failure.jsonl');
    const failing = await connectCodexCallback({ ...config, threadId: original, peers: ['allowed-peer'],
      log: failingLog }, 'existing-acp');
    fs.renameSync(failingLog + '.inbox', failingLog + '.saved');
    fs.writeFileSync(failingLog + '.inbox', 'not a directory');
    // Opening below a regular file reports ENOENT on Windows and ENOTDIR on
    // POSIX. Both must refuse acceptance before any native submission.
    assert.throws(() => failing.receive({ ...message, deliveryId: 'f'.repeat(64) }),
      { code: process.platform === 'win32' ? 'ENOENT' : 'ENOTDIR' });
    await failing.close();
    assert.equal(calls.filter(call => call.method === 'thread/queue/addOnce').length, beforeBounded);

    // Inject the typed cleanup failure after a real proxy RPC. This exercises
    // the public shared-connection callback and its durable filesystem inbox;
    // it does not claim to reproduce an OS process that cannot be reaped.
    for (const closeDuringDelivery of [true, false]) {
      const shared = await connectCodex(config);
      const cleanupLog = path.join(directory, `cleanup-${closeDuringDelivery}.jsonl`);
      const cleanupError = new CodexCleanupUnknownError('Fixture proxy cleanup remains unconfirmed');
      let release!: () => void, submitted = false;
      const barrier = new Promise<void>(resolve => { release = resolve; });
      const callback = createCodexCallback(async (method, params) => {
        await shared.request(method, params);
        submitted = true;
        await barrier;
        throw cleanupError;
      }, { threadId: original, peers: ['allowed-peer'], log: cleanupLog }, 'existing-acp');
      const before = calls.filter(call => call.method === 'thread/queue/addOnce').length;
      try {
        callback.receive({ ...message, deliveryId: (closeDuringDelivery ? '8' : '9').repeat(64) });
        await waitFor(() => submitted);
        const inbox = cleanupLog + '.inbox';
        const names = fs.readdirSync(inbox).filter(name => name.endsWith('.json'));
        assert.equal(names.length, 1);
        const pendingPath = path.join(inbox, names[0]);
        const beforeBytes = fs.readFileSync(pendingPath);
        assert.equal(JSON.parse(beforeBytes.toString()).state, 'pending');
        const closing = closeDuringDelivery ? callback.close() : undefined;
        release();
        if (!closing) {
          await waitFor(() => fs.readFileSync(cleanupLog, 'utf8').includes('"phase":"pending"'));
          // Let the ordinary one-second retry period pass before close.
          await new Promise(resolve => setTimeout(resolve, 1100));
        }
        await assert.rejects(closing ?? callback.close(), error => error === cleanupError);
        await assert.rejects(callback.close(), error => error === cleanupError);
        assert.deepEqual(fs.readFileSync(pendingPath), beforeBytes,
          'cleanup failure preserves the original pending ID, bytes and attempt count');
        assert.equal(calls.filter(call => call.method === 'thread/queue/addOnce').length, before + 1,
          'cleanup UNKNOWN stops delivery scheduling without replay');
        assert.throws(() => callback.receive(message), /closed/);
      } finally {
        release();
        await callback.close().catch(error => { if (error !== cleanupError) throw error; });
        await shared.close(); // This fixture's actual returned process is reaped.
      }
    }
    state = 'notLoaded';
    await assert.rejects(connectCodexCallback({ ...config, threadId: original, peers: [],
      log: path.join(directory, 'unloaded.jsonl') }, 'existing-acp'), /already be loaded/);
    assert.equal(calls.filter(call => call.method === 'turn/start' || call.method === 'turn/steer' || call.method === 'thread/queue/add').length, 0);
    assert.equal((calls.find(call => call.method === 'initialize')?.params?.clientInfo as { name: string }).name, 'scode-acp-client');
    assert.ok(!calls.some(call => /thread\/(start|resume|fork)|session\/prompt/.test(call.method)),
      'transport extraction does not create a controller, resume a thread or issue an ACP task prompt');
    console.log('PASS native Codex proxy processes: shared inventory/events, request-ID and observer isolation, typed outcomes, bounded exit, durable callback restart, stable-ID recovery and proxy reconnection');
  } finally {
    for (const socket of server.clients) socket.terminate();
    await new Promise<void>(resolve => server.close(() => resolve()));
    fs.rmSync(directory, { recursive: true, force: true });
  }
}
void main().catch(error => { console.error(error); process.exitCode = 1; });
