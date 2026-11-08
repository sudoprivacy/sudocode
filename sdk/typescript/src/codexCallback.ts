import { appendFileSync } from 'node:fs';
import * as path from 'node:path';
import { connectRecoveringCodex, validateCodexConnectionConfig, type CodexConnection, type CodexConnectionConfig } from './codexConnection';
import type { ScodeConnection, ScodePeerMessage } from './scodeAcp';
import { requireScodeNodeIdentity, type ScodeA2aResult } from './scodeA2a';
import { enqueueCodexPeerNotification, validateCodexNativeQueueConfig, type CodexNativeQueueConfig,
  type CodexNativeQueueReceipt } from './codexNativeQueue';
import { codexPeerData } from './codexPeerData';
import { openCodexInbox } from './codexInbox';

export interface CodexCallbackConfig extends CodexConnectionConfig {
  threadId: string;
  peers: string[];
  log: string;
  /** Both modes use durable idempotent queue ingress for one original thread. */
  transport?: 'socket' | 'native-queue';
  nativeHome?: string;
  onProducerObservation?: CodexNativeQueueConfig['onProducerObservation'];
}

/** Receives the owned scode's peer messages. Its scode connection must own peer
 * input (`onPeerMessage`), so the Codex frontend is the only respondent. */
export interface CodexCallback {
  receive(message: ScodePeerMessage): void;
  close(): Promise<void>;
}

type Requester = CodexConnection['request'];

/** The caller owns/resumes the frontend. A callback never resumes or forks it. */
export async function forwardScodeToCodex(
  request: Requester, threadId: string, message: ScodePeerMessage, cursor: string,
): Promise<{ method: string; queueItemId: string }> {
  const { text } = codexPeerData(message, cursor);
  // addOnce persists before replying and keeps its ID receipt after dequeue.
  // Never fall back to turn/start, steer or non-idempotent add on older hosts.
  const result = await request<{ queuedSubmission: { id: string; clientUserMessageId: string;
    input: Array<{ type: string; text?: string }> } }>('thread/queue/addOnce', {
    threadId, input: [{ type: 'text', text }], clientUserMessageId: cursor,
  });
  const item = result?.queuedSubmission;
  if (!item || typeof item.id !== 'string' || !item.id || item.clientUserMessageId !== cursor
      || item.input?.length !== 1 || item.input[0].type !== 'text' || item.input[0].text !== text) {
    throw new Error('Native durable queue returned a different delivery receipt');
  }
  return { method: 'thread/queue/addOnce', queueItemId: item.id };

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
    return createFilteredCallback(config, sessionId, (message, eventId) =>
      enqueueCodexPeerNotification({ ...config, nativeHome }, config.threadId, message, eventId));
  }
  log({ phase: 'connecting', threadId: config.threadId, sessionId });
  const native = await connectRecoveringCodex(config);
  const close = async () => { closed = true; await native.close(); };
  let closed = false;
  try {
    const { thread } = await native.request<{ thread: { id: string; status: { type: string } } }>('thread/read', { threadId: config.threadId, includeTurns: false });
    if (thread.id !== config.threadId || !['active', 'idle'].includes(thread.status.type)) {
      throw new Error('Original callback thread must already be loaded; no automatic resume/fork');
    }
  } catch (error) { await close(); throw error; }
  let delivery: CodexCallback;
  try { delivery = createCodexCallback(native.request, config, sessionId); }
  catch (error) { await close(); throw error; }
  return { receive(message) { if (closed) throw new Error('Callback owner closed; retain upstream delivery'); delivery.receive(message); },
    async close() { await delivery.close(); await close(); } };
}

/** Share an existing native connection without creating another proxy owner. */
export function createCodexCallback(request: Requester,
  config: Pick<CodexCallbackConfig, 'threadId' | 'peers' | 'log'>, sessionId: string): CodexCallback {
  return createFilteredCallback(config, sessionId, (message, eventId) =>
    forwardScodeToCodex(request, config.threadId, message, eventId));
}

/** Explicit network-wide reception after the host verified this session's
 * Node identity. It accepts peer DATA, never tool authority, and changes none
 * of the existing send policy, permission checks or fixed-session allowlists.
 */
export function createCodexNodeCallback(request: Requester,
  config: Pick<CodexCallbackConfig, 'threadId' | 'peers' | 'log'>, connection: ScodeConnection,
  nodeIdentity: ScodeA2aResult): CodexCallback {
  requireScodeNodeIdentity(connection,nodeIdentity);
  return createFilteredCallback(config,connection.sessionId,(message,eventId)=>
    forwardScodeToCodex(request,config.threadId,message,eventId),'node-namespace');
}

/** One admission/filter/provenance/lifecycle lane for both native ingress modes. */
function createFilteredCallback(config: Pick<CodexCallbackConfig, 'threadId' | 'peers' | 'log'>, sessionId: string,
  forward: (message: ScodePeerMessage, eventId: string) => Promise<{ method: string; queueItemId: string } | CodexNativeQueueReceipt>,
  peerPolicy: 'allowlist' | 'node-namespace' = 'allowlist'): CodexCallback {
  const admitted = (message: ScodePeerMessage) => {
    const from = message.sudocode?.a2a?.from;
    return message.sessionId === sessionId && typeof from === 'string' && !!from && !from.includes('\0')
      && (peerPolicy === 'node-namespace' || config.peers.includes(from));
  };
  const inbox = openCodexInbox(config.log, config.threadId, sessionId, async (message, eventId) => {
    if (!admitted(message)) throw new Error('Recovered message no longer matches this owner policy');
    return forward(message, eventId);
  }, entry => appendFileSync(config.log, JSON.stringify({ at: new Date().toISOString(), peerPolicy, ...entry })+'\n', { mode: 0o600 }));
  return {
    receive(message) { if (admitted(message)) inbox.receive(message); },
    close: () => inbox.close(),
  };
}
