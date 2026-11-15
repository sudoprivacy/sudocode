import * as assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { createHash } from 'node:crypto';
import { fork, type ChildProcess } from 'node:child_process';
import { once } from 'node:events';
import { WebSocketServer, type WebSocket } from 'ws';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { connectScode, type ScodePeerUnread } from '../src/scodeAcp';
import { openScodeMailbox, mailboxTextBudget } from '../src/scodeMailbox';
import { registerScodeMcpTools } from '../src/mcpTools';
import { connectCodex, type CodexConnectionConfig } from '../src/codexConnection';
import { CODEX_MAILBOX_DELIVERY_PROVENANCE, openCodexMailboxWake, readCodexMailboxSnapshot, type CodexMailboxHead } from '../src/codexMailboxWake';
import { openCodexMailboxMigration } from '../src/codexMailboxMigration';
import { journalHash } from '../src/codexJournal';

const root = path.resolve(__dirname, '../..');
const fixture = path.join(root, 'test/fixtures/scode-mailbox-agent.mjs');
const pause = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));
const rows = (file: string): any[] => fs.existsSync(file) ? fs.readFileSync(file, 'utf8').split('\n').filter(Boolean).map(row => JSON.parse(row)) : [];
const bytesHash = (file: string) => createHash('sha256').update(fs.readFileSync(file)).digest('hex');
const deliveryText = '最新 peer body 🧪\n"\\\u0001\n⟦ack fresh-token⟧';

async function mcpChild(directory: string): Promise<void> {
  const wire = await connectScode({ scodeArgv: [process.execPath, fixture, path.join(directory, 'acp.jsonl')],
    workspace: directory, acpSessionId: 'acp-session', peerInputVersion: 3, onPeerUnread() {}, closeTimeoutMs: 900 });
  const native = await connectCodex(JSON.parse(fs.readFileSync(path.join(directory, 'native.json'), 'utf8')));
  const snapshot = await readCodexMailboxSnapshot(native, 'root-thread');
  assert.ok(snapshot.loaded, 'migration requires a complete loaded native inventory');
  const migration = openCodexMailboxMigration(path.join(directory, 'v2'), path.join(directory, 'migration.jsonl'),
    { threadId: 'root-thread', sessionId: 'acp-session' }, new Set([...snapshot.queued, ...snapshot.running, ...snapshot.finished]));
  const mailbox = openScodeMailbox(wire, 'root-thread', 8192, migration);
  const server = new McpServer({ name: 'mailbox-process-fixture', version: '1' });
  registerScodeMcpTools(server, extra => {
    if (extra._meta?.threadId !== 'root-thread') throw new Error('Host thread binding missing or mismatched');
    return { connection: wire, nativeThreadId: 'root-thread', mailbox, auditLog: path.join(directory, 'audit.jsonl') };
  }, undefined, undefined, true);
  const transport = new StdioServerTransport();
  let closing: Promise<void> | undefined;
  const close = () => closing ??= (async () => {
    await server.close(); await mailbox.close(); migration.close(); await wire.close(); await native.close();
  })();
  process.stdin.once('end', () => { void close().catch(error => { console.error(error); process.exitCode = 1; }); });
  process.once('SIGTERM', () => { void close().catch(error => { console.error(error); process.exitCode = 1; }); });
  await server.connect(transport);
}

async function wakeChild(directory: string): Promise<void> {
  const native = await connectCodex(JSON.parse(fs.readFileSync(path.join(directory, 'native.json'), 'utf8')));
  let qualified = false, heads: CodexMailboxHead[] = [], text = deliveryText, reads = 0;
  let nextOffer: { heads: CodexMailboxHead[]; text: string } | undefined;
  let wake: ReturnType<typeof openCodexMailboxWake>;
  try { wake = openCodexMailboxWake({ actor: 'actor-one', threadId: 'root-thread', native,
    journal: path.join(directory, 'wake.jsonl'), encodedSubmissionBytes: 8192, qualified: async () => qualified,
    offer: async () => reads++ > 0 && nextOffer ? nextOffer : { heads, text } }); }
  catch (error) { await native.close(); throw error; }
  process.on('message', (input: { id: number; kind: string; qualified?: boolean; heads?: CodexMailboxHead[];
    text?: string; nextOffer?: { heads: CodexMailboxHead[]; text: string } }) => {
    void (async () => {
      if (input.kind === 'set') {
        qualified = input.qualified!; heads = input.heads!; text = input.text ?? deliveryText;
        nextOffer = input.nextOffer; reads = 0;
      }
      else if (input.kind === 'evaluate') await wake.evaluate();
      else if (input.kind === 'close') { await wake.close(); await native.close(); }
      else throw new Error('Unknown process fixture command');
      process.send!({ id: input.id, result: wake.status() });
      if (input.kind === 'close') process.disconnect();
    })().catch(error => { process.send!({ id: input.id, error: String(error) }); });
  });
  process.send!({ ready: true });
}

/** Real child ACP, MCP and official-proxy transport boundaries; this fixture
 * is not stock-Codex/model presentation qualification or a scode lease test. */
async function main(): Promise<void> {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'scode-mailbox-process-'));
  fs.chmodSync(directory, 0o700);
  const children = new Set<ChildProcess>();
  const nativeServer = new WebSocketServer({ host: '127.0.0.1', port: 0 });
  await once(nativeServer, 'listening');
  const address = nativeServer.address();
  assert.ok(address && typeof address !== 'string');
  const environment: NodeJS.ProcessEnv = { ...process.env, SCODE_NATIVE_ENV_SENTINEL: 'owned-proxy' };
  delete environment.CODEX_THREAD_ID;
  const config: CodexConnectionConfig = { codexArgv: [process.execPath, path.join(root, 'test/fixtures/codex-stdio-proxy.mjs'), String(address.port), 'normal'],
    socket: 'fixture-native.sock', env: environment as Record<string, string>, closeTimeoutMs: 900 };
  fs.writeFileSync(path.join(directory, 'native.json'), JSON.stringify(config), { mode: 0o600 });
  const calls: Array<{ method: string; params?: any }> = [];
  let loaded = true, status = 'idle', repeatCursor = false, loseReply = false, loseRequest = false;
  let queue: Array<{ id: string; clientUserMessageId: string }> = [];
  let turns: Array<{ id: string; status: string; items: object[] }> = [];
  const legacyCustody = '11111111-1111-1111-1111-111111111111';
  const legacyClean = '22222222-2222-2222-2222-222222222222';
  const legacyUnresolved = '33333333-3333-3333-3333-333333333333';
  const makeTurn = (id: string, clientId: string | null, state = 'completed') => ({ id, status: state,
    items: [{ type: 'userMessage', clientId, content: [{ type: 'text', text: 'input' }] }] });
  const nativeCalls = () => calls.filter(row => row.method === 'thread/queue/add');
  let nativeFailure: unknown;
  nativeServer.on('connection', (socket: WebSocket) => socket.on('message', bytes => {
    try {
      const call = JSON.parse(bytes.toString());
      if (!call.method) return;
      calls.push(call);
      if (call.method === 'initialized') return;
      let result: unknown;
      if (call.method === 'initialize') result = { userAgent: 'mailbox-native-wire-fixture' };
      else if (call.method === 'thread/loaded/list') result = call.params.cursor ? { data: loaded ? ['root-thread'] : [], nextCursor: null }
        : { data: ['other-thread'], nextCursor: 'loaded-two' };
      else if (call.method === 'thread/queue/list') result = call.params.cursor ? { data: queue, nextCursor: repeatCursor ? 'queue-two' : null }
        : { data: [], nextCursor: 'queue-two' };
      else if (call.method === 'thread/turns/list') {
        assert.equal(call.params.itemsView, 'full'); assert.equal(call.params.sortDirection, 'desc');
        result = call.params.cursor ? { data: turns.slice(1), nextCursor: null }
          : { data: turns.slice(0, 1), nextCursor: 'turns-two' };
      } else if (call.method === 'thread/read') result = { thread: { id: 'root-thread', status: { type: status }, turns: turns.filter(row => row.status === 'inProgress') } };
      else if (call.method === 'thread/queue/add') {
        // The cross-process observer must see the slot before native accepts it.
        const slots = rows(path.join(directory, 'wake.jsonl'));
        assert.equal(slots.at(-1).value.clientId, call.params.clientUserMessageId);
        assert.equal(call.params.threadId, 'root-thread');
        assert.equal(call.params.input[0].text, CODEX_MAILBOX_DELIVERY_PROVENANCE + '\n' + deliveryText,
          'queue receives the fresh offer verbatim, including Unicode and opaque tokens');
        assert.ok(Buffer.byteLength(JSON.stringify(call.params)) <= 8192);
        const submission = { id: `native-item-${nativeCalls().length}`, clientUserMessageId: call.params.clientUserMessageId };
        if (!loseRequest) queue.push(submission);
        else loseRequest = false;
        if (loseReply) { loseReply = false; socket.terminate(); return; }
        result = { queuedSubmission: submission };
      } else throw new Error(`Unexpected native method ${call.method}`);
      socket.send(JSON.stringify({ jsonrpc: '2.0', id: call.id, result }));
    } catch (error) { nativeFailure = error; socket.terminate(); }
  }));
  const startWake = async () => {
    const child = fork(__filename, ['--wake-child', directory], { stdio: ['ignore', 'ignore', 'pipe', 'ipc'], env: process.env });
    children.add(child);
    let stderr = '', serial = 0;
    child.stderr!.on('data', bytes => { stderr += String(bytes); });
    const exited = once(child, 'exit');
    const waiting = new Map<number, { resolve(value: any): void; reject(error: Error): void; timer: ReturnType<typeof setTimeout> }>();
    const ready = new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error(`Wake child startup timeout: ${stderr}`)), 10000);
      child.on('message', (message: any) => {
        if (message.ready) { clearTimeout(timer); resolve(); }
        else {
          const waiter = waiting.get(message.id);
          if (waiter) { waiting.delete(message.id); clearTimeout(waiter.timer); message.error ? waiter.reject(new Error(message.error)) : waiter.resolve(message.result); }
        }
      });
      child.once('exit', code => { clearTimeout(timer); reject(new Error(`Wake child exit ${code}: ${stderr}`));
        for (const waiter of waiting.values()) { clearTimeout(waiter.timer); waiter.reject(new Error(stderr)); } waiting.clear(); });
    });
    await ready;
    const request = (kind: string, more: object = {}) => new Promise<any>((resolve, reject) => {
      const id = ++serial, timer = setTimeout(() => reject(new Error(`Wake child ${kind} timeout: ${stderr}`)), 25000);
      waiting.set(id, { resolve, reject, timer }); child.send({ id, kind, ...more });
    });
    return { request, async close() { await request('close'); await exited; children.delete(child); } };
  };
  try {
    const notifications: ScodePeerUnread[] = [], errors: unknown[] = [];
    const directLog = path.join(directory, 'direct.jsonl');
    const direct = await connectScode({ scodeArgv: [process.execPath, fixture, directLog], workspace: directory,
      acpSessionId: 'acp-session', peerInputVersion: 3, onPeerUnread: value => { notifications.push(value); }, onError: value => { errors.push(value); } });
    try {
      await direct.request('_fixture/unread', {});
      await direct.request('_fixture/unread', { sessionId: 'other-session' });
      assert.deepEqual(notifications, [{ sessionId: 'acp-session', unread: 2 }]);
      assert.equal(errors.length, 1, 'another session cannot wake this binding');
    } finally { await direct.close(); }
    const oldLog = path.join(directory, 'older.jsonl');
    await assert.rejects(connectScode({ scodeArgv: [process.execPath, fixture, oldLog, 'older'], workspace: directory,
      acpSessionId: 'acp-session', peerInputVersion: 3, onPeerUnread() {} }), /peer input|peerInput/i);
    assert.equal(rows(oldLog).filter(row => row.method === 'session/load').length, 0, 'refuse before loading a v2 peer');

    fs.mkdirSync(path.join(directory, 'v2'), { mode: 0o700 });
    const oldFiles: string[] = [];
    for (const [index, eventId] of [legacyCustody, legacyClean, legacyUnresolved].entries()) {
      const message = { sessionId: 'acp-session', text: '原始 legacy 🧪', body: 'body', sudocode: { a2a: { from: 'peer', kind: 'message' } } };
      const file = path.join(directory, 'v2', eventId + '.json'); oldFiles.push(file);
      fs.writeFileSync(file, JSON.stringify({ version: 1, threadId: 'root-thread', sessionId: 'acp-session', eventId,
        messageHash: journalHash(message), message, receivedAt: 1, sequence: index + 1, state: 'pending', attempts: index === 1 ? 0 : 1 }), { mode: 0o600 });
    }
    const originalHashes = oldFiles.map(bytesHash);
    turns = [makeTurn('irrelevant', 'other-id'), makeTurn('custodied', legacyCustody)];
    const session = async (check: (client: Client) => Promise<void>) => {
      const transport = new StdioClientTransport({ command: process.execPath, args: [__filename, '--mcp-child', directory], stderr: 'pipe' });
      let stderr = ''; transport.stderr?.on('data', bytes => { stderr += String(bytes); });
      const client = new Client({ name: 'mailbox-process-client', version: '1' });
      try { await client.connect(transport); await check(client); }
      catch (error) { throw new Error(`${String(error)}\n${stderr}`); }
      finally { await client.close(); }
    };
    const call = (client: Client, input: object, threadId: string | undefined = 'root-thread') => client.callTool({
      name: 'scode_inbox', arguments: input as Record<string, unknown>, ...(threadId === undefined ? {} : { _meta: { threadId } }),
    });
    await session(async client => {
      assert.ok((await client.listTools()).tools.some(tool => tool.name === 'scode_inbox'));
      assert.equal((await call(client, {}, 'wrong-root')).isError, true);
      assert.equal((await client.callTool({ name: 'scode_inbox', arguments: {} })).isError, true);
      assert.equal((await call(client, { actor: 'injected' })).isError, true);
      assert.equal(rows(path.join(directory, 'acp.jsonl')).filter(row => row.method === 'offer-start').length, 0);
      const parallel = await Promise.all([call(client, {}), call(client, {})]);
      const offered = rows(path.join(directory, 'acp.jsonl')).filter(row => row.method.startsWith('offer-'));
      assert.deepEqual(offered.map(row => row.method), ['offer-start', 'offer-end', 'offer-start', 'offer-end']);
      assert.deepEqual(parallel[0].content, [{ type: 'text', text: offered[1].response.text }], 'MCP preserves exact UTF-8 presentation');
      assert.equal(offered[3].response.frames[0].redelivery, true);
      assert.deepEqual(offered[0].params.legacy.map((entry: any) => [entry.eventId, entry.possiblyDelivered]), [[legacyClean, false], [legacyUnresolved, true]],
        'native history page two closes custody; unresolved entries retain their marker and order');
      assert.equal(offered[0].params.maxBytes, mailboxTextBudget(8192));
      assert.equal(offered[0].params.ack, undefined, 'reading does not synthesize acknowledgements');
      assert.match(JSON.stringify(await call(client, { read: 'window' })), /fixture-next/);
      assert.equal((await call(client, { read: 'window', window: 1 })).isError, true);
      assert.match(JSON.stringify(await call(client, { read: 'window', window: 1, after: 'fixture-next' })), /fixture-window-ack/);
      const minimum = await call(client, { read: 'error' });
      const diagnostic = JSON.parse((minimum.content as any[])[0].text);
      assert.equal(minimum.isError, true); assert.equal(diagnostic.outcome, 'unknown');
      assert.deepEqual(diagnostic.rpcError, { code: -32602, data: { reason: 'max_bytes_too_small', minimum: 12345 } });
      assert.equal(diagnostic.automaticRetry, false);
      assert.equal((await call(client, { read: 'oversize' })).isError, true, 'oversize presentation is refused, never cut');
      await call(client, { ack: ['refuse'] });
      await call(client, { ack: [`legacy-ack:${legacyClean}`] });
      assert.deepEqual(rows(path.join(directory, 'migration.jsonl')).map(row => row.value.state), ['native_custody', 'offered', 'offered', 'committed']);
    });
    await session(async client => {
      await call(client, {});
      const last = rows(path.join(directory, 'acp.jsonl')).filter(row => row.method === 'offer-start').at(-1);
      assert.deepEqual(last.params.legacy.map((entry: any) => entry.eventId), [legacyUnresolved], 'restart continues the persisted cutover');
      await call(client, { ack: [`legacy-ack:${legacyUnresolved}`] });
      await call(client, {});
      assert.equal(rows(path.join(directory, 'acp.jsonl')).filter(row => row.method === 'offer-start').at(-1).params.legacy, undefined);
    });
    assert.deepEqual(oldFiles.map(bytesHash), originalHashes, 'all original v2 receipt bytes remain untouched');
    assert.ok(!fs.readFileSync(path.join(directory, 'migration.jsonl'), 'utf8').includes('原始'), 'new ledger holds no peer bodies');

    // A lost ACP reply never causes an automatic replay; a timed-out live
    // transport is poisoned so a second call cannot overlap its unknown ACK.
    for (const failure of ['lost', 'timeout']) {
      const log = path.join(directory, `${failure}.jsonl`);
      const wire = await connectScode({ scodeArgv: [process.execPath, fixture, log], workspace: directory,
        acpSessionId: 'acp-session', peerInputVersion: 3, onPeerUnread() {} });
      const mailbox = openScodeMailbox(wire, 'root-thread', 8192);
      try {
        await assert.rejects(mailbox.inbox({ read: failure, ack: ['fixture-ack'] }));
        await assert.rejects(mailbox.inbox({}));
        assert.equal(rows(log).filter(row => row.method === 'offer-start').length, 1);
      } finally { await mailbox.close(); await wire.close(); }
    }

    queue = []; turns = []; const head = { conversation: 'c1', start: 10, deliveryId: 'head-one' };
    let owner = await startWake();
    await owner.request('set', { qualified: false, heads: [head] });
    assert.equal((await owner.request('evaluate')).state, 'native_unqualified'); assert.equal(nativeCalls().length, 0);
    await owner.request('set', { qualified: true, heads: [head] });
    loaded = false; assert.equal((await owner.request('evaluate')).state, 'waiting'); assert.equal(nativeCalls().length, 0);
    loaded = true; repeatCursor = true;
    assert.equal((await owner.request('evaluate')).state, 'blocked'); assert.equal(nativeCalls().length, 0);
    repeatCursor = false; status = 'systemError';
    assert.equal((await owner.request('evaluate')).state, 'blocked'); assert.equal(nativeCalls().length, 0);
    status = 'active';
    await owner.request('set', { qualified: true, heads: [head], text: '\u0001'.repeat(2000) });
    const tooLarge = await owner.request('evaluate');
    assert.equal(tooLarge.state, 'blocked'); assert.match(tooLarge.error, /exceeded its qualified budget/);
    assert.equal(nativeCalls().length, 0); assert.equal(rows(path.join(directory, 'wake.jsonl')).length, 0,
      'JSON escaping and provenance are bounded before reserving a submission; no token is cut');
    await owner.request('set', { qualified: true, heads: [head], text: 'stale-body', nextOffer: { heads: [], text: '' } });
    assert.equal((await owner.request('evaluate')).state, 'waiting');
    assert.equal(nativeCalls().length, 0); assert.equal(rows(path.join(directory, 'wake.jsonl')).length, 0,
      'a head retired during native observation consumes no submission');
    await owner.request('set', { qualified: true, heads: [head], text: 'stale-body', nextOffer: { heads: [head], text: deliveryText } });
    assert.equal((await owner.request('evaluate')).state, 'submitted', 'active thread uses its queue, never steering');
    const firstId = nativeCalls()[0].params.clientUserMessageId;
    assert.equal((await owner.request('evaluate')).state, 'waiting', 'queue page two blocks duplicate wakes');
    await owner.close(); owner = await startWake(); await owner.request('set', { qualified: true, heads: [head] });
    assert.equal((await owner.request('evaluate')).state, 'waiting'); assert.equal(nativeCalls().length, 1);
    queue = []; turns = [makeTurn('wake-running', firstId, 'inProgress')];
    assert.equal((await owner.request('evaluate')).state, 'waiting'); assert.equal(nativeCalls().length, 1);
    turns = [makeTurn('wake-finished', firstId)]; status = 'idle'; loseReply = true;
    assert.equal((await owner.request('evaluate')).state, 'blocked'); assert.equal(nativeCalls().length, 2);
    await owner.close(); owner = await startWake(); await owner.request('set', { qualified: true, heads: [head] });
    assert.equal((await owner.request('evaluate')).state, 'waiting', 'lost reply is reconciled from native queue after restart');
    const secondId = nativeCalls()[1].params.clientUserMessageId;
    queue = []; turns.unshift(makeTurn('wake-two', secondId));
    assert.equal((await owner.request('evaluate')).state, 'submitted'); const thirdId = nativeCalls()[2].params.clientUserMessageId;
    queue = []; turns.unshift(makeTurn('wake-three', thirdId));
    assert.equal((await owner.request('evaluate')).state, 'delivery_exhausted'); assert.equal(nativeCalls().length, 3);
    await owner.close(); owner = await startWake(); await owner.request('set', { qualified: true, heads: [head] });
    assert.equal((await owner.request('evaluate')).state, 'delivery_exhausted'); assert.equal(nativeCalls().length, 3);
    turns.unshift(makeTurn('fresh-human-turn', null));
    assert.equal((await owner.request('evaluate')).state, 'submitted'); assert.equal(nativeCalls().length, 4);
    const fourthId = nativeCalls()[3].params.clientUserMessageId;
    assert.notEqual(fourthId, firstId); assert.ok(fourthId.endsWith(':1'), 'a new native non-wake epoch opens one new key');
    queue = []; turns.unshift(makeTurn('wake-four', fourthId));
    await owner.request('set', { qualified: true, heads: [{ ...head, start: 100, deliveryId: 'head-two' }] });
    loseRequest = true;
    assert.equal((await owner.request('evaluate')).state, 'submitted'); assert.equal(nativeCalls().length, 5);
    const unresolvedAt = performance.now();
    assert.equal((await owner.request('evaluate')).state, 'waiting');
    await pause(60500);
    await owner.request('evaluate');
    assert.ok(performance.now() - unresolvedAt >= 60000, 'use the real policy delay, not a fake clock');
    assert.equal(nativeCalls().length, 6, 'unresolved submission spends the next number after 60 s idle');
    assert.notEqual(nativeCalls()[5].params.clientUserMessageId, nativeCalls()[4].params.clientUserMessageId);
    assert.ok(nativeCalls()[5].params.clientUserMessageId.endsWith(':2'));
    await owner.request('set', { qualified: false, heads: [head] });
    assert.equal((await owner.request('evaluate')).state, 'native_unqualified'); assert.equal(nativeCalls().length, 6);
    await owner.close();
    assert.ok(calls.every(row => !['turn/start', 'turn/steer', 'thread/queue/start', 'thread/start', 'thread/resume', 'thread/queue/addOnce'].includes(row.method)));
    assert.equal(new Set(nativeCalls().map(row => row.params.clientUserMessageId)).size, nativeCalls().length);
    assert.ok(!fs.readFileSync(path.join(directory, 'wake.jsonl'), 'utf8').includes('fresh-token'),
      'submission journal persists identity and budget, not peer presentation');
    assert.equal(nativeFailure, undefined);
    fs.appendFileSync(path.join(directory, 'wake.jsonl'), '{torn');
    await assert.rejects(startWake(), /incomplete tail/);
    assert.equal(nativeCalls().length, 6, 'a damaged log cannot reuse a previously sent number');
    console.log('PASS mailbox process boundaries: exact v3 ACP/MCP, strict host selection, serialized opaque ACKs/windows, typed errors, response loss, read-only v2 migration, paged native reconciliation, persisted three-slot wake cap, actual 60 s delay. Native/model qualification remains separate.');
  } finally {
    for (const child of children) if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
    await Promise.allSettled([...children].filter(child => child.exitCode === null && child.signalCode === null).map(child => once(child, 'exit')));
    for (const socket of nativeServer.clients) socket.terminate();
    await new Promise<void>((resolve, reject) => nativeServer.close(error => error ? reject(error) : resolve()));
    assert.equal(path.dirname(directory), path.resolve(os.tmpdir())); assert.ok(path.basename(directory).startsWith('scode-mailbox-process-'));
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

const run = process.argv[2] === '--mcp-child' ? mcpChild(process.argv[3])
  : process.argv[2] === '--wake-child' ? wakeChild(process.argv[3]) : main();
void run.catch(error => { console.error(error); process.exitCode = 1;
  // Startup refusal has no active controller; end the fixture process so its
  // parent can record failure and join it, instead of masking a refused log.
  if (process.argv[2] === '--wake-child') process.exit(1);
});
