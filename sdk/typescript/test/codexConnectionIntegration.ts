import * as assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { once } from 'node:events';
import { WebSocketServer } from 'ws';
const root = path.resolve(__dirname, '../..');
const { connectCodex, connectCodexCallback, CodexRequestError } = require(process.argv[2] ?? path.join(root, 'dist/index.cjs')) as typeof import('../src/index');

async function main(): Promise<void> {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'scode-codex-connection-'));
  const server = new WebSocketServer({ host: '127.0.0.1', port: 0 });
  await once(server, 'listening');
  const address = server.address();
  assert.ok(address && typeof address !== 'string');
  const config = { codexArgv: [process.execPath, path.join(root, 'test/fixtures/codex-stdio-proxy.mjs'), String(address.port), 'normal'],
    socket: 'fixture-native.sock' };
  const calls: Array<{ method: string; params?: Record<string, unknown> }> = [];
  const clientReplies: unknown[] = [];
  const original = process.env.CODEX_THREAD_ID ?? 'fixture-original';
  let state = 'idle';
  let connections = 0;
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
      const result = call.method === 'initialize' ? { userAgent: 'native-protocol-fixture/0.163.0' }
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
      assert.deepEqual(native.initialized, { userAgent: 'native-protocol-fixture/0.163.0' });
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

    const notification = { sessionId: 'existing-acp', update: { sessionUpdate: 'user_message_chunk' as const,
      content: { type: 'text' as const, text: 'peer data 零 🧪' } }, _meta: { sudocode: { a2a: { from: 'allowed-peer' } } } };
    for (const mode of ['idle', 'active']) {
      state = mode;
      const log = path.join(directory, `${mode}.jsonl`);
      const callback = await connectCodexCallback({ ...config, threadId: original, peers: ['allowed-peer'], log }, 'existing-acp');
      try {
        callback.receive({ ...notification, sessionId: 'different-acp' });
        callback.receive({ ...notification, _meta: { sudocode: { a2a: { from: 'unlisted-peer' } } } });
        callback.receive(notification);
        await waitFor(() => fs.readFileSync(log, 'utf8').includes('"phase":"accepted"'));
        const entries = fs.readFileSync(log, 'utf8').trim().split('\n').map(line => JSON.parse(line));
        assert.equal(entries.filter(entry => entry.phase === 'received').length, 1);
        const accepted = entries.find(entry => entry.phase === 'accepted');
        assert.equal(accepted.method, mode === 'idle' ? 'turn/start' : 'turn/steer');
      } finally { await callback.close(); }
    }
    state = 'notLoaded';
    await assert.rejects(connectCodexCallback({ ...config, threadId: original, peers: [],
      log: path.join(directory, 'unloaded.jsonl') }, 'existing-acp'), /already be loaded/);
    assert.equal(calls.filter(call => call.method === 'turn/start').length, 1);
    assert.equal(calls.filter(call => call.method === 'turn/steer').length, 1);
    assert.equal(calls.find(call => call.method === 'turn/steer')?.params?.expectedTurnId, 'running');
    const idle = calls.find(call => call.method === 'turn/start')?.params;
    assert.deepEqual(idle?.input, []);
    assert.equal((idle?.toolOutput as { namespace: string }).namespace, 'nexus');
    assert.equal((calls.find(call => call.method === 'initialize')?.params?.clientInfo as { name: string }).name, 'scode-acp-client');
    assert.ok(!calls.some(call => /thread\/(start|resume|fork)|session\/prompt/.test(call.method)),
      'transport extraction does not create a controller, resume a thread or issue an ACP task prompt');
    console.log('PASS native Codex proxy processes: shared inventory/events, request-ID and observer isolation, typed outcomes, bounded exit, unchanged callback and no loss retry');
  } finally {
    for (const socket of server.clients) socket.terminate();
    await new Promise<void>(resolve => server.close(() => resolve()));
    fs.rmSync(directory, { recursive: true, force: true });
  }
}
void main().catch(error => { console.error(error); process.exitCode = 1; });
