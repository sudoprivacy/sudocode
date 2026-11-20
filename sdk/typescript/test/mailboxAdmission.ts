import assert from 'node:assert/strict';
import { codexMailboxAdmission, CODEX_MAILBOX_PRESENTATION_BYTES } from '../src/codexMailboxAdmission';
import { requestCodexMailboxPage } from '../src/codexMailboxWake';
import { CodexRequestError, type CodexConnection } from '../src/codexConnection';

type Reply = (method: string, params: any) => unknown;

function server(reply: Reply) {
  const calls: string[] = [];
  const state = { initialized: {} as object | undefined, connected: true, reply };
  const native: CodexConnection = {
    get initialized() { return state.initialized; },
    get connected() { return state.connected; },
    lastNotificationError: undefined,
    exited: new Promise(() => {}),
    async request<T>(method: string, params: unknown): Promise<T> {
      calls.push(method);
      return state.reply(method, params) as T;
    },
    async close() {},
  };
  return { native, calls, state };
}

const stock: Reply = (method, params) => {
  if (method === 'thread/read') return { thread: { id: params.threadId, status: { type: 'idle' }, turns: [] } };
  // Item contents belong to each inventory read, not to admission.
  if (method === 'thread/queue/list') return { data: [{ id: 'typed-by-user' }], nextCursor: null };
  if (method === 'thread/turns/list') return { data: [], nextCursor: null, backwardsCursor: null };
  throw new Error(`unexpected ${method}`);
};

async function main(): Promise<void> {
  const admission = codexMailboxAdmission();
  assert.equal(admission.encodedResultBytes, CODEX_MAILBOX_PRESENTATION_BYTES);
  assert.equal(admission.encodedSubmissionBytes, CODEX_MAILBOX_PRESENTATION_BYTES);

  const a = server(stock);
  assert.equal(await admission.qualified(a.native, 'root'), true);
  assert.deepEqual(a.calls, ['thread/queue/list', 'thread/turns/list', 'thread/read']);
  assert.equal(await admission.qualified(a.native, 'root'), true);
  assert.equal(a.calls.length, 3, 'one probe per server connection and thread');
  assert.equal(await admission.qualified(a.native, 'other'), true);
  assert.equal(a.calls.length, 6);

  // A reconnect presents a new initialize response: the new server is probed.
  a.state.initialized = {};
  a.state.reply = method => { throw Object.assign(new Error(`${method}: method not found`), { code: -32601 }); };
  assert.equal(await admission.qualified(a.native, 'root'), false);
  a.state.reply = stock;
  assert.equal(await admission.qualified(a.native, 'root'), true, 'a refusal is not remembered');

  const offline = server(stock);
  offline.state.connected = false;
  assert.equal(await admission.qualified(offline.native, 'root'), false);
  assert.deepEqual(offline.calls, []);
  offline.state.connected = true; offline.state.initialized = undefined;
  assert.equal(await admission.qualified(offline.native, 'root'), false);

  const otherThread = server((method, params) => method === 'thread/read'
    ? { thread: { id: 'substituted' } } : stock(method, params));
  assert.equal(await admission.qualified(otherThread.native, 'root'), false);

  const malformed = server((method, params) => method === 'thread/turns/list' ? { turns: [] } : stock(method, params));
  assert.equal(await admission.qualified(malformed.native, 'root'), false);

  // Actual 0.171 response to listing a new, still-unmaterialized TUI.
  const rpc = { code: -32600, message: 'thread root is not materialized yet; thread/turns/list is unavailable before first user message' };
  const newThread = server((method, params) => {
    if (method === 'thread/turns/list') throw new CodexRequestError('unknown', 'native refused history', rpc);
    return stock(method, params);
  });
  assert.equal(await admission.qualified(newThread.native, 'root'), true);
  assert.equal(await admission.qualified(newThread.native, 'other'), false);
  await assert.rejects(requestCodexMailboxPage(newThread.native, 'thread/turns/list', { threadId: 'root', limit: 1, cursor: 'next' }));
  rpc.code = -32601;
  assert.equal(await codexMailboxAdmission().qualified(newThread.native, 'root'), false);

  // A probe that straddles a reconnect proves nothing about the new server.
  let reconnects = 1;
  const racing = server((method, params) => {
    if (method === 'thread/read' && reconnects-- > 0) racing.state.initialized = {};
    return stock(method, params);
  });
  assert.equal(await admission.qualified(racing.native, 'root'), false);
  assert.equal(await admission.qualified(racing.native, 'root'), true);
  console.log('mailbox admission: ok');
}

main().catch(error => { console.error(error); process.exitCode = 1; });
