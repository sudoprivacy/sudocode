import * as acp from '@agentclientprotocol/sdk';
import { appendFileSync } from 'node:fs';
import { Readable, Writable } from 'node:stream';

// Wire fixture only: tokens are deliberately fixed, with no claim to test
// scode's mailbox lease, renderer or HMAC. Those require the real Rust CLI.
const [journal, scenario = 'normal'] = process.argv.slice(2);
const log = entry => appendFileSync(journal, JSON.stringify({ pid: process.pid, ...entry }) + '\n');
let sessionId, offered = 0, retired = false;
const app = acp.agent({ name: 'mailbox-wire-fixture', version: '1' })
  .onRequest(acp.methods.agent.initialize, ctx => {
    const version = ctx.params.clientCapabilities?._meta?.sudocode?.peerInput?.version;
    log({ method: 'initialize', version, attachment: JSON.parse(process.env.SUDOCODE_CODEX_PEER_ATTACHMENT ?? 'null') });
    return { protocolVersion: 1, agentCapabilities: { loadSession: true },
      _meta: { sudocode: { peerInput: { version: scenario === 'older' ? 2 : version } } } };
  })
  .onRequest(acp.methods.agent.session.load, ctx => {
    sessionId = ctx.params.sessionId; log({ method: 'session/load', sessionId }); return {};
  })
  .onRequest('_fixture/unread', value => value, async ctx => {
    await ctx.client.notify('_sudocode/session/peer_unread', { sessionId: ctx.params.sessionId ?? sessionId, unread: 2 });
    return {};
  })
  .onRequest('_sudocode/session/peer_offer', value => value, async ctx => {
    log({ method: 'offer-start', params: ctx.params });
    if (ctx.params.sessionId !== sessionId || ctx.params.threadId !== 'root-thread') {
      throw new acp.RequestError(-32602, 'Wrong binding', { reason: 'binding_mismatch' });
    }
    const read = ctx.params.read;
    if (read?.frameId === 'error') throw new acp.RequestError(-32602, 'Cannot fit window', { reason: 'max_bytes_too_small', minimum: 12345 });
    if (read?.frameId === 'lost') { process.exit(27); }
    if (read?.frameId === 'timeout') return new Promise(() => {});
    // Async response ordering is visible in the original wire journal.
    await new Promise(resolve => setTimeout(resolve, 25));
    let text = '⟦frame conversation:0-100⟧\n同一消息 🧪\n"\\\u0001\n⟦ack fixture-ack⟧';
    const commits = [];
    if (ctx.params.ack?.includes('fixture-ack')) {
      commits.push({ conversation: 'conversation', from: 0, to: 100, status: retired ? 'already' : 'committed' });
      retired = true;
    }
    for (const entry of ctx.params.legacy ?? []) {
      if (ctx.params.ack?.includes(`legacy-ack:${entry.eventId}`)) commits.push({ legacyEventId: entry.eventId, status: 'committed' });
    }
    if (ctx.params.ack?.includes('refuse')) commits.push({ conversation: 'conversation', from: 0, to: 100,
      status: 'refused', reason: 'lease_lost' });
    if (read?.frameId === 'oversize') text = 'x'.repeat(ctx.params.maxBytes + 1);
    if (read?.frameId === 'window') text = read.window === 0 ? '窗口 🧪\n⟦next fixture-next⟧' : '最后一窗\n⟦ack fixture-window-ack⟧';
    const response = { text, commits, frames: retired ? [] : [{ frameId: 'conversation:0-100', conversation: 'conversation',
      deliveryIds: ['delivery-one'], bytes: 100, window: read?.frameId === 'window' ? { index: read.window, count: 2 } : null,
      complete: read?.frameId !== 'window' || read.window === 1, redelivery: offered++ > 0 }], unread: retired ? 0 : 1 };
    log({ method: 'offer-end', response }); return response;
  })
  .onRequest(acp.methods.agent.session.close, () => { log({ method: 'session/close' }); return {}; });
const connection = app.connect(acp.ndJsonStream(Writable.toWeb(process.stdout), Readable.toWeb(process.stdin)));
await connection.closed;
