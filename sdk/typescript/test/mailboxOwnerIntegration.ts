import * as assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { createHash } from 'node:crypto';
import { once } from 'node:events';
import { WebSocketServer } from 'ws';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { journalHash } from '../src/codexJournal';
import { registerScodeMcpTools } from '../src/mcpTools';
import { CODEX_MAILBOX_DELIVERY_PROVENANCE } from '../src/codexMailboxWake';

const sdk = path.resolve(__dirname, '../..');
const threadId = '33333333-3333-3333-3333-333333333333';
const { connectCodexAutojoin } = require(path.join(sdk, 'dist/index.cjs')) as typeof import('../src/index');
const rows = (file: string): any[] => fs.existsSync(file) ? fs.readFileSync(file, 'utf8').split('\n').filter(Boolean).map(row => JSON.parse(row)) : [];
const hash = (file: string) => createHash('sha256').update(fs.readFileSync(file)).digest('hex');
const pause = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));

async function child(directory: string) {
  const server = new McpServer({ name: 'mailbox-owner-fixture', version: '1' });
  const owner = await connectCodexAutojoin({ native: JSON.parse(fs.readFileSync(path.join(directory, 'native.json'), 'utf8')),
    stateDirectory: path.join(directory, 'state'), peers: [], peerPolicy: 'node-namespace', pollIntervalMs: 100000,
    onError: error => console.error('Fixture owner observation:', error),
    // Private fixture admission only; this file is not a production certificate.
    mailbox: { encodedResultBytes: 8192, encodedSubmissionBytes: 4096,
      qualified: async () => fs.readFileSync(path.join(directory, 'qualified'), 'utf8') === 'yes' },
    async resolveAgent(claim) {
      fs.appendFileSync(path.join(directory, 'enrollment.jsonl'), JSON.stringify(claim) + '\n');
      const configFile = path.join(directory, 'profile.json');
      fs.writeFileSync(configFile, JSON.stringify({ scodeArgv: [process.execPath,
        path.join(sdk, 'test/fixtures/scode-direct-a2a-agent.mjs'), 'mailbox'], workspace: claim.workspace,
        acpSessionId: 'acp-session', closeTimeoutMs: 900,
        env: { SCODE_DIRECT_FIXTURE_ACTOR: claim.actor, HYDRA_DIRECT_FIXTURE_JOURNAL: path.join(directory, 'acp.jsonl') },
      }), { flag: 'wx', mode: 0o600 });
      return { actor: claim.actor, configFile };
    },
  });
  registerScodeMcpTools(server, async extra => {
    await owner.refresh();
    return { connection: owner.connectionFor(extra._meta?.threadId), mailbox: owner.mailboxFor(extra._meta?.threadId),
      nativeThreadId: extra._meta?.threadId as string, auditLog: path.join(directory, 'audit.jsonl') };
  }, undefined, () => ({ agents: owner.status() }), true);
  let closing: Promise<void> | undefined;
  const close = () => closing ??= (async () => { await server.close(); await owner.close(); })();
  process.stdin.once('end', () => { void close().catch(error => { console.error(error); process.exitCode = 1; }); });
  process.once('SIGTERM', () => { void close().catch(error => { console.error(error); process.exitCode = 1; }); });
  await server.connect(new StdioServerTransport());
}

async function main() {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'scode-mailbox-owner-'));
  fs.chmodSync(directory, 0o700);
  const stateDirectory = path.join(directory, 'state'), callbackLog = path.join(stateDirectory, `${threadId}.callback.jsonl`);
  fs.mkdirSync(callbackLog + '.inbox', { recursive: true, mode: 0o700 });
  fs.writeFileSync(path.join(directory, 'qualified'), 'no', { mode: 0o600 });
  const legacy = ['11111111-1111-1111-1111-111111111111', '22222222-2222-2222-2222-222222222222',
    '55555555-5555-5555-5555-555555555555'];
  const oldFiles = legacy.map((eventId, i) => {
    const message = { sessionId: 'acp-session', text: 'original legacy body', body: 'original legacy body',
      sudocode: { a2a: { from: 'peer', kind: 'message' } } };
    const file = path.join(callbackLog + '.inbox', eventId + '.json');
    fs.writeFileSync(file, JSON.stringify({ version: 1, threadId, sessionId: 'acp-session', eventId,
      messageHash: journalHash(message), message, receivedAt: 1, sequence: i + 1, state: 'pending', attempts: i ? 0 : 1 }), { mode: 0o600 });
    return file;
  });
  const originalHashes = oldFiles.map(hash);
  const endpoint = new WebSocketServer({ host: '127.0.0.1', port: 0 });
  await once(endpoint, 'listening');
  const address = endpoint.address(); assert.ok(address && typeof address !== 'string');
  const env: NodeJS.ProcessEnv = { ...process.env, SCODE_NATIVE_ENV_SENTINEL: 'owned-proxy' }; delete env.CODEX_THREAD_ID;
  const native = { codexArgv: [process.execPath, path.join(sdk, 'test/fixtures/codex-stdio-proxy.mjs'), String(address.port), 'normal'],
    socket: 'fixture-native.sock', env: env as Record<string, string>, closeTimeoutMs: 900 };
  fs.writeFileSync(path.join(directory, 'native.json'), JSON.stringify(native), { mode: 0o600 });
  let loaded = true, failure: unknown;
  const calls: any[] = [], queue: any[] = [];
  endpoint.on('connection', socket => socket.on('message', bytes => {
    try {
      const call = JSON.parse(bytes.toString()); calls.push(call);
      if (call.id === undefined) return;
      let result: unknown;
      if (call.method === 'initialize') result = { userAgent: 'unqualified-fixture-without-addOnce' };
      else if (call.method === 'thread/loaded/list') result = { data: loaded ? [threadId] : [], nextCursor: null };
      else if (call.method === 'thread/read') result = { thread: { id: threadId, cwd: directory,
        status: { type: 'idle' }, parentThreadId: null, threadSource: 'user', ephemeral: false, canAcceptDirectInput: true, turns: [] } };
      else if (call.method === 'thread/turns/list') result = call.params.cursor ? { data: [{ id: 'old-native-turn', status: 'completed',
        items: [{ type: 'userMessage', clientId: legacy[0] }] }], nextCursor: null } : { data: [], nextCursor: 'page-two' };
      else if (call.method === 'thread/queue/list') result = { data: queue, nextCursor: null };
      else if (call.method === 'thread/queue/add') {
        assert.equal(rows(callbackLog + '.wake.jsonl').at(-1).value.clientId, call.params.clientUserMessageId);
        const presentation = rows(path.join(directory, 'acp.jsonl')).filter(row => row.method === 'peer_offer_result').at(-1).response.text;
        assert.equal(call.params.input[0].text, CODEX_MAILBOX_DELIVERY_PROVENANCE + '\n' + presentation);
        assert.ok(Buffer.byteLength(JSON.stringify(call.params)) <= 4096, 'independent submission budget includes provenance');
        queue.push({ id: 'queue-item-' + queue.length, clientUserMessageId: call.params.clientUserMessageId });
        result = { queuedSubmission: queue.at(-1) };
      } else throw new Error(`Unexpected native method ${call.method}`);
      socket.send(JSON.stringify({ jsonrpc: '2.0', id: call.id, result }));
    } catch (error) { failure = error; socket.terminate(); }
  }));
  let client: Client | undefined;
  const open = async () => {
    const transport = new StdioClientTransport({ command: process.execPath, args: [__filename, '--child', directory], stderr: 'inherit' });
    client = new Client({ name: 'mailbox-owner-client', version: '1' }); await client.connect(transport);
  };
  const call = (name: string, args: object = {}, selectedThread = threadId) => client!.callTool({ name,
    arguments: args as Record<string, unknown>, _meta: { threadId: selectedThread } });
  const status = async () => JSON.parse(((await call('scode_status')).content as any[])[0].text).agents;
  const offers = () => rows(path.join(directory, 'acp.jsonl')).filter(row => row.method === 'peer_offer');
  const waitQueue = async (size: number) => {
    const deadline = performance.now() + 5000;
    while (queue.length < size && performance.now() < deadline) await pause(20);
    assert.equal(queue.length, size);
  };
  try {
    await assert.rejects(connectCodexAutojoin({ native, stateDirectory, peers: [],
      mailbox: { encodedResultBytes: 8192, encodedSubmissionBytes: 4096, qualified: async () => true },
      async resolveAgent() { throw new Error('must not enroll'); } }),
    /Invalid Codex/, 'v3 must not silently broaden the legacy allowlist');
    await open();
    assert.equal((await status()).length, 1, 'the native fixture supplies one valid loaded root');
    assert.equal((await status())[0].state, 'native_unqualified');
    assert.equal((await call('scode_inbox')).isError, true);
    assert.equal(rows(path.join(directory, 'enrollment.jsonl')).length, 0);
    assert.equal(rows(path.join(directory, 'acp.jsonl')).length, 0, 'unqualified native starts no receiver');
    fs.writeFileSync(path.join(directory, 'qualified'), 'yes');
    await pause(5100);
    assert.equal((await call('scode_agent_list')).isError, false);
    await waitQueue(1);
    assert.equal(rows(path.join(directory, 'enrollment.jsonl')).length, 1);
    assert.equal(rows(path.join(directory, 'acp.jsonl')).find(row => row.method === 'initialize').peerOwned, 3);
    assert.deepEqual(rows(callbackLog + '.migration.jsonl').map(row => row.value.state), ['native_custody', 'offered', 'offered']);
    assert.deepEqual(offers()[0].params.legacy.map((entry: any) => entry.eventId), legacy.slice(1));
    assert.equal(rows(callbackLog + '.wake.jsonl')[0].value.key.conversation, `legacy:${legacy[1]}`,
      'only the first pending legacy entry is known to fit the fresh bounded offer');
    assert.ok(offers()[0].params.maxBytes < Math.floor(4096 / 6),
      'queue bound reserves provenance and identity overhead before worst-case JSON escaping');
    assert.equal((await call('scode_inbox', {}, '44444444-4444-4444-4444-444444444444')).isError, true);
    const offered = await call('scode_inbox');
    assert.deepEqual(offered.content, [{ type: 'text', text: `original legacy body\n⟦ack legacy-ack:${legacy[1]}⟧` }]);
    const secondLegacy = await call('scode_inbox', { ack: [`legacy-ack:${legacy[1]}`] });
    assert.deepEqual(secondLegacy.content, [{ type: 'text', text: `original legacy body\n⟦ack legacy-ack:${legacy[2]}⟧` }]);
    const peer = await call('scode_inbox', { ack: [`legacy-ack:${legacy[2]}`] });
    assert.deepEqual(peer.content, [{ type: 'text', text: 'Peer body 🧪\n⟦ack fixture-ack⟧' }]);
    await call('scode_inbox', { ack: ['fixture-ack'] });
    const deadline = performance.now() + 5000;
    while ((await status())[0].mailbox?.state !== 'idle' && performance.now() < deadline) await pause(20);
    assert.equal((await status())[0].mailbox.state, 'idle');
    assert.deepEqual(rows(callbackLog + '.migration.jsonl').map(row => row.value.state),
      ['native_custody', 'offered', 'offered', 'committed', 'committed']);
    assert.equal(queue.length, 1, 'reading and ACKing cannot spend another wake slot while the first is queued');
    assert.deepEqual(oldFiles.map(hash), originalHashes);
    fs.writeFileSync(path.join(directory, 'qualified'), 'no');
    const before = offers().length;
    assert.equal((await call('scode_inbox', { ack: ['fixture-ack'] })).isError, true);
    assert.equal((await status())[0].state, 'native_unqualified');
    assert.equal(offers().length, before, 'changed native/settings refuse before ACK reaches scode');
    await client!.close(); client = undefined;
    fs.writeFileSync(path.join(directory, 'qualified'), 'yes');
    await open();
    assert.equal((await call('scode_inbox')).isError, undefined);
    assert.equal(rows(path.join(directory, 'enrollment.jsonl')).length, 1, 'ready binding retains actor, operation and ACP session');
    assert.equal(offers().at(-1).params.legacy, undefined, 'restart retains completed migration outcomes');
    assert.equal(queue.length, 1, 'restart reconciles the existing native wake');
    loaded = false;
    assert.equal((await call('scode_inbox')).isError, true);
    assert.deepEqual(await status(), []);
    assert.equal(failure, undefined);
    assert.ok(!calls.some(call => ['thread/queue/addOnce', 'thread/queue/start', 'turn/start', 'turn/steer'].includes(call.method)));
    assert.deepEqual(oldFiles.map(hash), originalHashes);
    console.log('PASS composed v3 owner/MCP process: admission before enrollment/ACP, verified identity, immutable v2 migration, persisted wake, revoked qualification, ready-binding restart, unload cleanup. Fixture admission is not native qualification.');
  } finally {
    await client?.close();
    for (const socket of endpoint.clients) socket.terminate();
    await new Promise<void>((resolve, reject) => endpoint.close(error => error ? reject(error) : resolve()));
    assert.equal(path.dirname(directory), path.resolve(os.tmpdir())); assert.ok(path.basename(directory).startsWith('scode-mailbox-owner-'));
    fs.rmSync(directory, { recursive: true, force: true });
  }
}

void (process.argv[2] === '--child' ? child(process.argv[3]) : main()).catch(error => { console.error(error); process.exitCode = 1; });
