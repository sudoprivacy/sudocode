import * as assert from 'node:assert/strict';
import * as path from 'node:path';
import type { ScodeAcpConfig, SessionNotification, ScodeQuestionRequest } from '../src/scodeAcp';
const root = path.resolve(__dirname, '../..');
const { connectScode, validateScodeQuestionResponse } = require(path.join(root, 'dist/index.cjs')) as typeof import('../src/scodeAcp');

const fixture = path.join(root, 'test/fixtures/scode-acp-agent.mjs');
const base: ScodeAcpConfig = {
  scodeArgv: [process.execPath, fixture], workspace: root,
  acpSessionId: 'original-native-id', startupTimeoutMs: 2500, closeTimeoutMs: 200,
};
const delay = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));
const prompt = (sessionId: string, text: string) => ({ sessionId, prompt: [{ type: 'text', text }] });

async function main(): Promise<void> {
  const methods: string[] = [];
  const notices: SessionNotification[] = [];
  const runtime = await connectScode({ ...base, onProtocol(direction, message) {
    if (direction === 'outgoing' && message && typeof message === 'object' && 'method' in message) {
      methods.push(String(message.method));
    }
  } }, notification => notices.push(notification));
  try {
    assert.equal(runtime.sessionId, 'original-native-id');
    assert.ok(methods.includes('session/load'));
    assert.ok(!methods.includes('session/new'));
    const response = await runtime.request<{ stopReason: string }>('session/prompt', prompt(runtime.sessionId, 'hello'));
    assert.equal(response.stopReason, 'end_turn');
    assert.equal(notices.filter(n => n.update.sessionUpdate === 'user_message_chunk').length, 0);
    await delay(100);
    assert.equal(notices.filter(n => n.update.sessionUpdate === 'user_message_chunk').length, 1,
      'permanent notification handler must remain active after foreground prompt completion');
    const denied = await runtime.request<{ _meta: { permission: { outcome: { outcome: string } } } }>(
      'session/prompt', prompt(runtime.sessionId, 'permission'));
    assert.equal(denied._meta.permission.outcome.outcome, 'cancelled');
  } finally { await runtime.close(); }
  assert.equal(runtime.closed, true);
  await runtime.exited;
  await runtime.close(); // Close is idempotent.
  await assert.rejects(runtime.request('session/prompt', prompt(runtime.sessionId, 'closed')), /closed/);

  const allowed = await connectScode({ ...base, onPermission: async () => ({ outcome: { outcome: 'selected', optionId: 'allow-once' } }) });
  try {
    const response = await allowed.request<{ _meta: { permission: { outcome: { optionId: string } } } }>(
      'session/prompt', prompt(allowed.sessionId, 'permission'));
    assert.equal(response._meta.permission.outcome.optionId, 'allow-once');
  } finally { await allowed.close(); }

  const diagnostics: unknown[] = [];
  const forged = await connectScode({ ...base, onError: error => diagnostics.push(error),
    onPermission: async () => ({ outcome: { outcome: 'selected', optionId: 'never-offered' } }) });
  try {
    const response = await forged.request<{ _meta: { permission: { outcome: { outcome: string } } } }>(
      'session/prompt', prompt(forged.sessionId, 'permission'));
    assert.equal(response._meta.permission.outcome.outcome, 'cancelled');
    assert.equal(diagnostics.length, 1);
  } finally { await forged.close(); }

  const question = await connectScode({ ...base, onQuestion: async request => ({ answers: [
    { id: request.questions[0].id, value: 'revise' },
  ] }) });
  try {
    const response = await question.request<{ _meta: { answer: { answers: Array<{ value: string }> } } }>(
      'session/prompt', prompt(question.sessionId, 'question'));
    assert.equal(response._meta.answer.answers[0].value, 'revise');
  } finally { await question.close(); }

  const badQuestion = await connectScode({ ...base, onQuestion: async () => ({ answers: [{ id: 'action', value: 'unoffered' }] }) });
  try {
    // The SDK preserves the remote error boundary as an internal request error.
    await assert.rejects(badQuestion.request('session/prompt', prompt(badQuestion.sessionId, 'question')));
  } finally { await badQuestion.close(); }
  for (const kind of ['text','boolean','multi_select']) {
    const request: ScodeQuestionRequest = { sessionId: 's', toolCallId: 'q', questions: [{
      id: 'q', prompt: 'Question', kind, required: true, allowCustomInput: false,
      options: [{ label: 'One', value: 'one', recommended: false }],
    }] };
    assert.doesNotThrow(() => validateScodeQuestionResponse(request, { answers: [{ id: 'q', value: 'opaque native value' }] }));
  }

  for (const scenario of ['no-load', 'load-fails']) {
    const observed: string[] = [];
    await assert.rejects(connectScode({ ...base, scodeArgv: [process.execPath, fixture, scenario],
      onProtocol(direction, message) {
        if (direction === 'outgoing' && message && typeof message === 'object' && 'method' in message) {
          observed.push(String(message.method));
        }
      }, onError: () => {},
    }));
    assert.ok(!observed.includes('session/new'), 'failed native load must never create another session');
  }

  const started = Date.now();
  await assert.rejects(connectScode({ ...base, scodeArgv: [process.execPath, fixture, 'startup-hang'],
    startupTimeoutMs: 100, onError: () => {}, }), /timed out/);
  assert.ok(Date.now()-started < 2000, 'initialization timeout must not leak a live child');
  const owner = new AbortController();
  const abortStarted = Date.now();
  const initializing = connectScode({ ...base, scodeArgv: [process.execPath, fixture, 'startup-hang'],
    signal: owner.signal, startupTimeoutMs: 20000, onError: () => {} });
  setTimeout(() => owner.abort(), 100);
  await assert.rejects(initializing, /cancelled/);
  assert.ok(Date.now()-abortStarted < 2000, 'owner cancellation must interrupt initialization before its timeout');
  await assert.rejects(connectScode({ ...base, scodeArgv: [path.join(base.workspace, 'missing-agent-binary')],
    onError: () => {}, }), /ENOENT|exited/);

  const stubborn = await connectScode({ ...base, scodeArgv: [process.execPath, fixture, 'ignore-close'], onError: () => {} });
  const stopping = Date.now();
  await stubborn.close();
  assert.ok(Date.now()-stopping < 1500, 'explicit stop must be bounded when session/close and EOF are ignored');
  assert.equal((await stubborn.exited).signal, 'SIGKILL');
  const disconnected = await connectScode({ ...base, onError: () => {} });
  let transportTimer: ReturnType<typeof setTimeout> | undefined;
  try {
    await assert.rejects(Promise.race([
      disconnected.request('session/prompt', prompt(disconnected.sessionId, 'agent-exit')),
      new Promise((_, reject) => { transportTimer = setTimeout(() => reject(new Error('Transport closure was not observed')), 2000); }),
    ]), error => !String(error).includes('not observed'));
  } finally {
    clearTimeout(transportTimer);
    await disconnected.close();
  }
  assert.equal((await disconnected.exited).code, 17);
  assert.equal(disconnected.closed, true, 'unexpected agent exit must reject pending requests and close ownership');
  console.log('PASS scode ACP: native load, idle callbacks, permissions, questions, startup failure and bounded stop');
}
void main().catch(error => { console.error(error); process.exitCode = 1; });
