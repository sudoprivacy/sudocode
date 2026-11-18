import * as assert from 'node:assert/strict';
import * as fs from 'node:fs';
import * as os from 'node:os';
import * as path from 'node:path';
import { randomUUID } from 'node:crypto';
import { once } from 'node:events';
import { createServer } from 'node:net';
import { execFileSync } from 'node:child_process';
import { WebSocketServer } from 'ws';
import { Client } from '@modelcontextprotocol/sdk/client/index.js';
import { StdioClientTransport } from '@modelcontextprotocol/sdk/client/stdio.js';
import { CODEX_MAILBOX_DELIVERY_PROVENANCE } from '../src/codexMailboxWake';
import { CODEX_MAILBOX_PRESENTATION_BYTES } from '../src/codexMailboxAdmission';

// The production v3 path end to end: runtime JSON `peerInputVersion: 3`, the
// global `--codex` MCP entry and its owner process, a stock-shaped native
// server without durableQueueAddOnce, and a peerInput v3 scode.
const root = path.resolve(__dirname, '../..');
const entry = path.join(root, 'dist/server.cjs');
const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'codex-mailbox-mcp-'));
const threadId = randomUUID();
const calls: Array<{ method: string; id?: number; params?: any }> = [];
const queue: Array<{ id: string; clientUserMessageId: string }> = [];
let loaded = [threadId];
let failure: unknown;
let windowsOwnerTask: ReturnType<typeof import('../src/codexMcp')['prepareWindowsCodexOwner']> | undefined;
let mcp: Client | undefined;

const native = new WebSocketServer({ host: '127.0.0.1', port: 0 });
native.on('connection', socket => socket.on('message', bytes => {
  try {
    const call = JSON.parse(bytes.toString()); calls.push(call);
    if (call.id === undefined) return;
    const thread = { id: call.params?.threadId, cwd: directory, status: { type: 'idle' }, parentThreadId: null,
      threadSource: 'user', ephemeral: false, canAcceptDirectInput: true, turns: [] };
    let result: unknown;
    if (call.method === 'initialize') result = { userAgent: 'stock-shaped-fixture' };
    else if (call.method === 'thread/loaded/list') result = { data: loaded, nextCursor: null };
    else if (call.method === 'thread/read') result = { thread };
    else if (call.method === 'thread/queue/list') result = { data: queue, nextCursor: null };
    else if (call.method === 'thread/turns/list') result = { data: [], nextCursor: null };
    else if (call.method === 'thread/queue/add') {
      queue.push({ id: 'queue-item-' + queue.length, clientUserMessageId: call.params.clientUserMessageId });
      result = { queuedSubmission: queue.at(-1) };
    } else throw new Error(`Unexpected native method ${call.method}`);
    socket.send(JSON.stringify({ jsonrpc: '2.0', id: call.id, result }));
  } catch (error) { failure = error; socket.terminate(); }
}));

const text = (result: unknown) => ((result as { content: Array<{ text: string }> }).content)[0].text;
async function until(check: () => boolean | Promise<boolean>, ms: number, label: string): Promise<void> {
  const deadline = Date.now() + ms;
  while (!await check()) {
    assert.equal(failure, undefined);
    assert.ok(Date.now() < deadline, label);
    await new Promise(resolve => setTimeout(resolve, 25));
  }
}

async function main(): Promise<void> {
  await once(native, 'listening');
  const address = native.address(); assert.ok(address && typeof address !== 'string');
  const reservation = createServer(); reservation.listen(0, '127.0.0.1'); await once(reservation, 'listening');
  const reserved = reservation.address(); assert.ok(reserved && typeof reserved !== 'string');
  await new Promise<void>(resolve => reservation.close(() => resolve()));
  const env: Record<string, string> = { SCODE_NATIVE_ENV_SENTINEL: 'owned-proxy' };
  for (const [key, value] of Object.entries(process.env)) if (value !== undefined && key !== 'CODEX_THREAD_ID') env[key] = value;
  const config = path.join(directory, 'runtime.json');
  fs.writeFileSync(config, JSON.stringify({ version: 1, mode: 'codex-autojoin',
    native: { codexArgv: [process.execPath, path.join(root, 'test/fixtures/codex-stdio-proxy.mjs'), String(address.port), 'normal'],
      socket: 'fixture-native.sock', env, closeTimeoutMs: 600 },
    stateDirectory: path.join(directory, 'bindings'), auditLog: path.join(directory, 'audit.jsonl'), peers: [],
    peerPolicy: 'node-namespace', peerInputVersion: 3,
    enrollmentArgv: [process.execPath, path.join(root, 'test/fixtures/codexAgentEnrollment.cjs'), directory, 'mailbox'],
    endpoint: { port: reserved.port, token: '03'.repeat(32) } }), { mode: 0o600 });
  if (process.platform === 'win32') {
    const { prepareWindowsCodexOwner } = require(path.join(root, 'dist/index.cjs')) as typeof import('../src/index');
    windowsOwnerTask = prepareWindowsCodexOwner(config, entry);
    execFileSync(windowsOwnerTask.installationArgv[0], windowsOwnerTask.installationArgv.slice(1), { timeout: 10000, windowsHide: true });
  }
  mcp = new Client({ name: 'codex-mailbox-mcp', version: '1' }, { capabilities: {} });
  await mcp.connect(new StdioClientTransport({ command: process.execPath, args: [entry, '--codex', '--config', config], stderr: 'inherit' }));
  const status = async () => JSON.parse(text(await mcp!.callTool({ name: 'scode_status', arguments: {} })));
  await until(async () => (await status()).state === 'observing', 10000, 'MCP owner did not attach its native observer');
  assert.ok((await mcp.listTools()).tools.some(tool => tool.name === 'scode_inbox'), 'v3 presents scode_inbox');

  // An idle root with an unread frame is woken by one ordinary queued turn.
  await until(() => calls.some(call => call.method === 'thread/queue/add'), 10000, 'idle root was not woken');
  const probe = calls.find(call => call.method === 'thread/queue/list' && call.params?.limit === 1);
  assert.equal(probe?.params.threadId, threadId, 'admission probes the actual server for this thread');
  const adds = calls.filter(call => call.method === 'thread/queue/add');
  assert.equal(adds.length, 1);
  assert.equal(adds[0].params.threadId, threadId);
  assert.match(adds[0].params.clientUserMessageId, /^scode-wake:/);
  assert.ok(adds[0].params.input[0].text.startsWith(CODEX_MAILBOX_DELIVERY_PROVENANCE + '\n'));
  assert.match(adds[0].params.input[0].text, /Peer body/);
  assert.ok(Buffer.byteLength(JSON.stringify(adds[0].params)) <= CODEX_MAILBOX_PRESENTATION_BYTES);
  const agent = (await status()).agents.find((row: { threadId: string }) => row.threadId === threadId);
  assert.equal(agent?.state, 'connected');
  assert.equal(agent?.mailbox?.state, 'submitted');

  const offered = await mcp.callTool({ name: 'scode_inbox', arguments: {}, _meta: { threadId } });
  assert.equal(offered.isError, undefined, text(offered));
  assert.match(text(offered), /Peer body/);
  assert.ok(Buffer.byteLength(JSON.stringify(offered)) <= CODEX_MAILBOX_PRESENTATION_BYTES);
  const acked = await mcp.callTool({ name: 'scode_inbox', arguments: { ack: ['fixture-ack'] }, _meta: { threadId } });
  assert.equal(text(acked), 'No unread mailbox frames');
  assert.ok(!calls.some(call => ['thread/queue/addOnce', 'thread/queue/start', 'turn/start', 'turn/steer'].includes(call.method)),
    'the mailbox path queues ordinary input only');

  const ownerPid = (await status()).ownerPid as number;
  await mcp.close(); mcp = undefined;
  loaded = [];
  await until(() => { try { process.kill(ownerPid, 0); return false; } catch { return true; } }, 15000,
    'owner did not retire after its root unloaded');
  assert.equal(failure, undefined);
  console.log('PASS Codex v3 production path: runtime JSON admission, scode_inbox presentation, one queued wake and an explicit ACK on a server without durableQueueAddOnce');
}

void main().catch(error => { console.error(error); process.exitCode = 1; }).finally(async () => {
  loaded = [];
  await mcp?.close();
  for (const client of native.clients) client.terminate();
  await new Promise<void>(resolve => native.close(() => resolve()));
  if (windowsOwnerTask) execFileSync(windowsOwnerTask.removalArgv[0], windowsOwnerTask.removalArgv.slice(1), { timeout: 10000, windowsHide: true });
  fs.rmSync(directory, { recursive: true, force: true });
});
