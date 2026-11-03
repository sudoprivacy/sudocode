import * as acp from '@agentclientprotocol/sdk';
import { Readable, Writable } from 'node:stream';

// An actual SDK stdio peer, not a transcript parser or a simulated connection.
const scenario = process.argv[2] ?? 'normal';
let sessionId = 'fixture-native-session';
let cancelled = false;
const hang = () => new Promise(() => {});
const app = acp.agent({ name: 'hydra-acp-fixture', version: '1' })
  .onRequest(acp.methods.agent.initialize, async () => {
    if (scenario === 'startup-hang') return hang();
    return { protocolVersion: 1, agentCapabilities: { loadSession: scenario !== 'no-load' } };
  })
  .onRequest(acp.methods.agent.session.new, () => ({ sessionId }))
  .onRequest(acp.methods.agent.session.load, async ctx => {
    if (scenario === 'load-fails') throw new Error('Fixture refuses this native session');
    sessionId = ctx.params.sessionId;
    if (scenario === 'load-permission') {
      await ctx.client.request(acp.methods.client.session.requestPermission, {
        sessionId,
        toolCall: { toolCallId: 'startup-input', title: 'Startup input', status: 'pending', kind: 'other' },
        options: [{ optionId: 'reject-once', kind: 'reject_once', name: 'Reject once' }],
      });
    }
    return {};
  })
  .onNotification(acp.methods.agent.session.cancel, () => { cancelled = true; })
  .onRequest(acp.methods.agent.session.prompt, async ctx => {
    const text = ctx.params.prompt.filter(x => x.type === 'text').map(x => x.text).join('');
    if (text === 'agent-exit') process.exit(17);
    const meta = {};
    if (text === 'permission') {
      meta.permission = await ctx.client.request(acp.methods.client.session.requestPermission, {
        sessionId,
        toolCall: { toolCallId: 'fixture-edit', title: 'Fixture edit', status: 'pending', kind: 'edit' },
        options: [
          { optionId: 'allow-once', kind: 'allow_once', name: 'Allow once' },
          { optionId: 'reject-once', kind: 'reject_once', name: 'Reject once' },
        ],
      });
    }
    if (text === 'question') {
      meta.answer = await ctx.client.request('_scode/ask_user_question', {
        sessionId, toolCallId: 'fixture-question', title: 'Review plan',
        questions: [{ id: 'action', prompt: 'Choose an action', kind: 'single_select', required: true,
          allowCustomInput: false, options: [
            { label: 'Execute', value: 'execute', recommended: false },
            { label: 'Revise', value: 'revise', recommended: false },
          ] }],
      });
    }
    await ctx.client.notify(acp.methods.client.session.update, {
      sessionId, update: { sessionUpdate: 'agent_message_chunk', content: { type: 'text', text: 'Foreground result' } },
    });
    setTimeout(() => {
      void ctx.client.notify(acp.methods.client.session.update, {
        sessionId,
        update: { sessionUpdate: 'user_message_chunk', content: { type: 'text', text: 'Unsolicited idle peer event' } },
        _meta: { sudocode: { a2a: { from: 'fixture-peer', kind: 'message' } } },
      }).catch(() => {});
    }, 40);
    return { stopReason: cancelled ? 'cancelled' : 'end_turn', _meta: meta };
  })
  .onRequest(acp.methods.agent.session.close, () => {
    if (scenario === 'ignore-close') return hang();
    return {};
  });
const connection = app.connect(acp.ndJsonStream(Writable.toWeb(process.stdout), Readable.toWeb(process.stdin)));
if (scenario === 'ignore-close') {
  setInterval(() => {}, 1000);
} else {
  await connection.closed;
}
