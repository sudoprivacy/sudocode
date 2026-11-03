import { appendFileSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import * as path from 'node:path';
import { connectCodex, CodexRequestError, validateCodexConnectionConfig, type CodexConnection, type CodexConnectionConfig } from './codexConnection';
import type { SessionNotification, ScodeConnection } from './scodeAcp';
import { requireScodeNodeIdentity, type ScodeA2aResult } from './scodeA2a';
import { enqueueCodexPeerNotification, validateCodexNativeQueueConfig, type CodexNativeQueueConfig,
  type CodexNativeQueueReceipt } from './codexNativeQueue';
import { codexPeerData } from './codexPeerData';

export interface CodexCallbackConfig extends CodexConnectionConfig {
  threadId: string;
  peers: string[];
  log: string;
  /** Default socket mode is unchanged. Queue ingress targets one known legacy thread. */
  transport?: 'socket' | 'native-queue';
  nativeHome?: string;
  onProducerObservation?: CodexNativeQueueConfig['onProducerObservation'];
}

export interface CodexCallback {
  receive(notification: SessionNotification): void;
  close(): Promise<void>;
}

type Requester = CodexConnection['request'];

/** The caller owns/resumes the frontend. A callback never resumes or forks it. */
export async function forwardScodeToCodex(
  request: Requester, threadId: string, notification: SessionNotification, cursor: string,
): Promise<{ method: string; turnId: string }> {
  const { thread } = await request<{ thread: { id: string; status: { type: string } } }>('thread/read', { threadId, includeTurns: false });
  if (thread.id !== threadId || !['active', 'idle'].includes(thread.status.type)) {
    throw new Error('Original Codex thread is not loaded and available; no resume/fork attempted');
  }
  const { output, text } = codexPeerData(notification, cursor);
  if (thread.status.type === 'active') {
    const page = await request<{ data: Array<{ id: string; status: string }> }>('thread/turns/list', {
      threadId, limit: 1, sortDirection: 'desc', itemsView: 'summary',
    });
    const turn = page.data.find(turn => turn.status === 'inProgress');
    if (!turn) throw new Error('Active turn changed during callback; delivery not assumed');
    const result = await request<{ turnId: string }>('turn/steer', { threadId, expectedTurnId: turn.id,
      input: [{ type: 'text', text }],
    });
    return { method: 'turn/steer', turnId: result.turnId };
  }
  const result = await request<{ turn: { id: string } }>('turn/start', {
    threadId, input: [], toolOutput: { name: 'scode_acp_notification', namespace: 'nexus', output },
  });
  return { method: 'turn/start', turnId: result.turn.id };
}

/** Connect only to an existing native server using its official stdio proxy. */
export async function connectCodexCallback(config: CodexCallbackConfig, sessionId: string): Promise<CodexCallback> {
  validateCodexConnectionConfig(config);
  if (typeof config.threadId !== 'string' || !config.threadId
      || !Array.isArray(config.peers) || config.peers.some(peer => typeof peer !== 'string' || !peer)
      || typeof config.log !== 'string' || !path.isAbsolute(config.log)
      || (config.transport !== undefined && !['socket', 'native-queue'].includes(config.transport))) {
    throw new Error('Invalid Codex callback configuration');
  }
  if (process.env.CODEX_THREAD_ID && process.env.CODEX_THREAD_ID !== config.threadId) {
    throw new Error('Callback target differs from the owning CODEX_THREAD_ID');
  }
  const log = (entry: unknown) => appendFileSync(config.log, JSON.stringify({ at: new Date().toISOString(), ...entry as object })+'\n', { mode: 0o600 });
  if (config.transport === 'native-queue') {
    const nativeHome = validateCodexNativeQueueConfig(config as CodexNativeQueueConfig);
    log({ phase: 'queue_transport', transport: 'native-queue', threadId: config.threadId, sessionId, nativeHome });
    return createFilteredCallback(config, sessionId, (notification, eventId) =>
      enqueueCodexPeerNotification({ ...config, nativeHome }, config.threadId, notification, eventId));
  }
  log({ phase: 'connecting', threadId: config.threadId, sessionId });
  const native = await connectCodex(config);
  const close = async () => { closed = true; await native.close(); };
  let closed = false;
  try {
    const { thread } = await native.request<{ thread: { id: string; status: { type: string } } }>('thread/read', { threadId: config.threadId, includeTurns: false });
    if (thread.id !== config.threadId || !['active', 'idle'].includes(thread.status.type)) {
      throw new Error('Original callback thread must already be loaded; no automatic resume/fork');
    }
  } catch (error) { await close().catch(() => {}); throw error; }
  const delivery = createCodexCallback(native.request, config, sessionId);
  return { receive(notification) { if (!closed) delivery.receive(notification); },
    async close() { await delivery.close(); await close(); } };
}

/** Share an existing native connection without creating another proxy owner. */
export function createCodexCallback(request: Requester,
  config: Pick<CodexCallbackConfig, 'threadId' | 'peers' | 'log'>, sessionId: string): CodexCallback {
  return createFilteredCallback(config, sessionId, (notification, eventId) =>
    forwardScodeToCodex(request, config.threadId, notification, eventId));
}

/** Explicit network-wide reception after the host verified this session's
 * Node identity. It accepts peer DATA, never tool authority, and changes none
 * of the existing send policy, permission checks or fixed-session allowlists.
 */
export function createCodexNodeCallback(request: Requester,
  config: Pick<CodexCallbackConfig, 'threadId' | 'peers' | 'log'>, connection: ScodeConnection,
  nodeIdentity: ScodeA2aResult): CodexCallback {
  requireScodeNodeIdentity(connection,nodeIdentity);
  return createFilteredCallback(config,connection.sessionId,(notification,eventId)=>
    forwardScodeToCodex(request,config.threadId,notification,eventId),'node-namespace');
}

/** One admission/filter/provenance/lifecycle lane for both native ingress modes. */
function createFilteredCallback(config: Pick<CodexCallbackConfig, 'threadId' | 'peers' | 'log'>, sessionId: string,
  forward: (notification: SessionNotification, eventId: string) => Promise<{ method: string; turnId: string } | CodexNativeQueueReceipt>,
  peerPolicy: 'allowlist' | 'node-namespace' = 'allowlist'): CodexCallback {
  let closed = false;
  const log = (entry: unknown) => appendFileSync(config.log, JSON.stringify({ at: new Date().toISOString(), ...entry as object })+'\n', { mode: 0o600 });
  let queued = 0;
  let chain = Promise.resolve();
  return {
    receive(notification) {
      const meta = notification._meta?.sudocode as { a2a?: { from?: string } } | undefined;
      const from = meta?.a2a?.from;
      if (closed || notification.sessionId !== sessionId || notification.update.sessionUpdate !== 'user_message_chunk'
          || typeof from!=='string' || !from || from.includes('\0')
          || (peerPolicy==='allowlist' && !config.peers.includes(from))) return;
      const eventId = randomUUID();
      log({ eventId, phase: 'received', peerPolicy, notification });
      if (queued >= 64) { log({ eventId, phase: 'pending', reason: 'Callback queue full; no delivery attempted' }); return; }
      queued++;
      chain = chain.then(async () => {
        if(closed){log({eventId,phase:'pending',reason:'Callback owner closed before submission'});return;}
        const result = await forward(notification, eventId);
        log({ eventId, phase: result.method === 'thread/queue/add' ? 'queued' : 'accepted', ...result });
      }).catch(error => {
        // Never replay a turn/start or steer after a lost response.
        process.stderr.write(`Codex callback ${eventId} pending: ${String(error)}\n`);
        try { log({ eventId, phase: 'pending', error: String(error),
          ...(error instanceof CodexRequestError ? { outcome: error.outcome, nativeEvidence: error.rpcError } : {}),
        }); } catch { /* Original ACP protocol log is retained by its owner. */ }
      }).finally(() => { queued--; });
    },
    async close() { closed = true; await chain; },
  };
}
