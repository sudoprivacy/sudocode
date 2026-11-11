import * as path from 'node:path';
import { connectRecoveringCodex, CodexRequestError, type CodexConnection, type CodexConnectionConfig } from './codexConnection';
import { createCodexCallback, createCodexNodeCallback, type CodexCallback } from './codexCallback';
import { connectScode, ScodeCleanupUnknownError, type ScodeConnection, type ScodePeerMessage } from './scodeAcp';
import { listScodeAgents, requireScodeNodeIdentity, ScodeA2aError } from './scodeA2a';
import type { ScodeReceiveHealth } from './scodeReceiveHealth';
import type { CodexDeliveryStatus } from './codexInbox';
import { assertWorkspace, boundProfile, claimBinding, identifier, provisionBinding, readBindings,
  writeBinding, type CodexBinding as Binding, type CodexAgentClaim, type CodexWorkspaceBinding } from './codexBindings';
export type { CodexAgentClaim, CodexWorkspaceBinding } from './codexBindings';

/** The operator's existing enrollment path owns authorization and credentials.
 * Return a private, durable profile with an EXISTING native ACP session. Never
 * return Node/CA authority or create another identity after an ambiguous error.
 */
export interface CodexAgentProfile {
  actor: string;
  configFile: string;
  workspaceBinding?: CodexWorkspaceBinding;
}

/** Only the authorized adapter can attest that no issuance was submitted. */
export class CodexEnrollmentError extends Error {
  constructor(readonly outcome: 'not_submitted' | 'unknown', detail: string) {super(detail);}
}

export interface CodexRootInventory {
  loaded: readonly string[];
  roots: ReadonlyArray<{
    threadId: string; qualified: boolean; status: string | null;
    parentThreadId?: string | null; threadSource?: string | null;
    ephemeral?: boolean; canAcceptDirectInput?: boolean;
  }>;
}

export interface CodexAutojoinConfig {
  native: CodexConnectionConfig;
  stateDirectory: string;
  peers: string[];
  /** Explicitly admit peer data from the verified Node namespace; default is the fixed allowlist. */
  peerPolicy?: 'allowlist' | 'node-namespace';
  resolveAgent: (claim: Readonly<CodexAgentClaim>) => Promise<CodexAgentProfile>;
  pollIntervalMs?: number;
  onError?: (error: unknown) => void;
  onClosed?: () => void;
  /** Diagnostic observation of the validated snapshot; never controls admission. */
  onInventory?: (observation: CodexRootInventory) => void | Promise<void>;
}

interface Thread {
  id: string;
  cwd: string;
  parentThreadId?: string | null;
  threadSource?: string | null;
  ephemeral?: boolean;
  canAcceptDirectInput?: boolean;
  status: { type: string };
}
interface Owner {
  binding: Binding;
  connection?: ScodeConnection;
  callback?: CodexCallback;
  error?: string;
  retryAt?: number;
  verified?: boolean;
}
export interface CodexAutojoin {
  /** True only after closure or authoritative inventory with no qualified roots. */
  readonly canRetire: boolean;
  /** Host-supplied MCP metadata selects an already loaded root, never arguments. */
  connectionFor(threadId: unknown): ScodeConnection;
  status(): Array<{threadId: string; actor: string; state: string; error?: string;
    receiver?: {pid:number; executable:string; sessionId:string; input:ScodeReceiveHealth}; delivery?:CodexDeliveryStatus}>;
  refresh(): Promise<void>;
  close(): Promise<void>;
}

function rootThread(thread: Thread): boolean {
  // Neither cwd, source="cli", sessionId nor canAcceptDirectInput alone proves
  // root ownership. Feature workers may accept input too. Missing fields fail
  // closed instead of adopting a thread from an older native protocol.
  return thread.threadSource === 'user' && thread.parentThreadId === null
    && thread.ephemeral === false && thread.canAcceptDirectInput === true
    && ['idle', 'active'].includes(thread.status?.type);
}

/** One composition owner per native endpoint. The caller fences that owner;
 * individual MCP clients must not each run this inventory/receiver loop.
 * Lifetime is the loaded native root, including work that outlives a frontend.
 */
export async function connectCodexAutojoin(config: CodexAutojoinConfig): Promise<CodexAutojoin> {
  if (!path.isAbsolute(config.stateDirectory) || !Array.isArray(config.peers)
      || config.peers.some(peer => typeof peer !== 'string' || !peer)
      || (config.peerPolicy!==undefined && !['allowlist','node-namespace'].includes(config.peerPolicy))
      || typeof config.resolveAgent !== 'function'
      || (config.pollIntervalMs !== undefined && (!Number.isFinite(config.pollIntervalMs) || config.pollIntervalMs < 100))) {
    throw new Error('Invalid Codex autojoin configuration');
  }
  const bindings = readBindings(config.stateDirectory);
  const owners = new Map<string, Owner>();
  let closed = false;
  let chain = Promise.resolve();
  let refreshing: Promise<void> | undefined;
  let native: CodexConnection;
  let inventoryError: string | undefined;
  const report = (error: unknown) => { try { config.onError?.(error); } catch { /* Observer only. */ } };
  const save = (binding: Binding, create = false) => writeBinding(config.stateDirectory, binding, create);
  for (const binding of bindings.values()) {
    if (binding.state === 'provisioning') {
      binding.state = 'recovery_required';
      await save(binding);
    }
  }
  const stopOwner = async (owner: Owner) => {
    const results = await Promise.allSettled([owner.callback?.close(), owner.connection?.close()]);
    const failed = results.find(result => result.status === 'rejected');
    if (failed?.status === 'rejected') throw failed.reason;
  };
  const open = async (thread: Thread) => {
    const binding = await claimBinding(config.stateDirectory, bindings, { id: thread.id, cwd: thread.cwd, frontend: 'codex' });
    const owner: Owner = { binding };
    owners.set(thread.id, owner);
    const pending: Array<{ message: ScodePeerMessage; resolve(): void; reject(error: unknown): void }> = [];
    try {
      try {
        await provisionBinding(config.stateDirectory, binding, thread.cwd, claim => {
          if (closed) throw new CodexEnrollmentError('not_submitted', 'Owner closed before enrollment submission');
          return config.resolveAgent(claim);
        }, error => error instanceof CodexEnrollmentError && error.outcome === 'not_submitted');
      } catch (error) {
        if (binding.state === 'pending') owner.retryAt = performance.now() + 2000;
        throw error;
      }
      if (closed) return;
      const profile = boundProfile(binding);
      let callback: CodexCallback | undefined;

      // Codex is the agent its peers address: the owned scode hands each peer
      // message over instead of answering it with its own model.
      const connection = await connectScode({ ...profile, onPeerMessage: message => {
        if(callback) { callback.receive(message); return; }
        if(pending.length>=64) throw new Error('ACP identity verification pending; retain delivery upstream');
        return new Promise<void>((resolve,reject) => pending.push({message,resolve,reject}));
      } });
      owner.connection = connection;
      // Native audit proves which Agent was actually loaded. A copied profile
      // cannot quietly attach every root to the same credential/receiver.
      const result = await listScodeAgents(connection);
      requireScodeNodeIdentity(connection,result,binding.actor);
      const callbackConfig={threadId:thread.id,peers:config.peers,
        log:path.join(config.stateDirectory,`${thread.id}.callback.jsonl`)};
      callback = config.peerPolicy==='node-namespace'
        ? createCodexNodeCallback(native.request,callbackConfig,connection,result)
        : createCodexCallback(native.request,callbackConfig,connection.sessionId);
      owner.callback = callback;
      for(const item of pending.splice(0)) {
        try { callback.receive(item.message); item.resolve(); } catch(error) { item.reject(error); }
      }
      owner.verified = true;
      void connection.exited.then(exit => {
        if (closed || owners.get(thread.id) !== owner) return;
        owner.error ??= `Owned scode exited ${exit.code}/${exit.signal}; recovering the same session`;
        owner.retryAt = performance.now() + 5000;
      });
      if (closed) await stopOwner(owner);
    } catch (error) {
      owner.error = String(error);
      for(const item of pending.splice(0)) item.reject(error);
      let cleanupKnown = !(error instanceof ScodeCleanupUnknownError);
      try { await stopOwner(owner); } catch (cleanupError) { cleanupKnown = false; report(cleanupError); }
      if (cleanupKnown && binding.state === 'ready') owner.retryAt = performance.now() + 5000;
      report(error);
    }
  };
  const loadedIds = async () => {
    const loaded = new Set<string>();
    let cursor: string | undefined;
    do {
      const page = await native.request<{data: string[]; nextCursor: string | null}>('thread/loaded/list', {cursor});
      if (!Array.isArray(page.data) || page.data.some(id => !identifier(id))
          || (page.nextCursor !== null && typeof page.nextCursor !== 'string')) throw new Error('Invalid native root inventory');
      for (const id of page.data) loaded.add(id);
      if (page.nextCursor && page.nextCursor === cursor) throw new Error('Native inventory cursor did not advance');
      cursor = page.nextCursor ?? undefined;
    } while (cursor);
    return loaded;
  };
  const refresh = () => {
    if (refreshing) return refreshing;
    chain = chain.then(async () => {
      if (closed) return;
      const loaded = await loadedIds();
      const threads = new Map<string, Thread>();
      for (const id of loaded) {
        if (closed) return;
        let thread: Thread;
        try {
          ({thread} = await native.request<{thread: Thread}>('thread/read', {threadId:id,includeTurns:false}));
        } catch (error) {
          const rpc = error instanceof CodexRequestError ? error.rpcError as {code?: unknown; message?: unknown} : undefined;
          // Native unload can race this read after a successful inventory.
          // Only an exact native refusal plus fresh confirmed absence permits
          // retirement. Other failures still invalidate the whole snapshot.
          // These are read-only observations; no command/enrollment is retried.
          if (rpc?.code !== -32600 || rpc.message !== `thread not loaded: ${id}`
              || (await loadedIds()).has(id)) throw error;
          loaded.delete(id);
          continue;
        }
        if (thread.id !== id || !path.isAbsolute(thread.cwd)) throw new Error('Native inventory identity mismatch');
        threads.set(id,thread);
      }
      const observed = [...threads.values()].map(thread => ({
        threadId:thread.id,qualified:rootThread(thread),status:thread.status?.type ?? null,
        parentThreadId:thread.parentThreadId,threadSource:thread.threadSource,
        ephemeral:thread.ephemeral,canAcceptDirectInput:thread.canAcceptDirectInput,
      }));
      const qualified = new Set(observed.filter(thread=>thread.qualified).map(thread=>thread.threadId));
      for (const [id, owner] of owners) {
        if (!loaded.has(id) || (owner.retryAt!==undefined && performance.now()>=owner.retryAt)) {
          await stopOwner(owner); owners.delete(id);
        }
      }
      for (const [id,thread] of threads) {
        if (closed) break;
        const owner = owners.get(id);
        if (owner?.error) continue;
        if (!qualified.has(id)) {
          if (owner) {await stopOwner(owner);owners.delete(id);}
        } else if (owner) {
          try {
            assertWorkspace(owner.binding,thread.cwd);
            if (owner.binding.state === 'ready') boundProfile(owner.binding);
          }
          catch (error) {owner.error=String(error);await stopOwner(owner);report(error);}
        } else await open(thread);
      }
      inventoryError = undefined;
      // Run after policy decisions with primitive-only copies. An observer
      // cannot rewrite the classification used by this refresh or break it.
      try {void Promise.resolve(config.onInventory?.({loaded:[...loaded],roots:observed})).catch(report);}
      catch (error) {report(error);}
    }).catch(error=>{inventoryError=String(error);report(error);});
    refreshing = chain.finally(() => { refreshing = undefined; });
    return refreshing;
  };
  native = await connectRecoveringCodex(config.native, notification => {
    if (['thread/started', 'thread/closed'].includes(notification.method)) void refresh();
  });
  const timer = setInterval(() => { void refresh(); }, config.pollIntervalMs ?? 1000);
  timer.unref();
  await refresh();
  let closePromise: Promise<void> | undefined;
  const close = () => closePromise ??= (async()=>{
    closed = true;
    clearInterval(timer);
    await chain;
    try {
      const results = await Promise.allSettled([...owners.values()].map(stopOwner));
      results.push(...await Promise.allSettled([native.close()]));
      const error = results.find(result => result.status === 'rejected');
      if(error?.status==='rejected')throw error.reason;
    } finally {
      try {config.onClosed?.();} catch(error) {report(error);}
    }
  })();

  return {
    get canRetire() {return closed || (!inventoryError && owners.size === 0);},
    refresh,
    connectionFor(threadId) {
      if (!identifier(threadId)) throw new ScodeA2aError('Native MCP threadId metadata is required; no sender argument or cwd inference', 'not_submitted');
      if(inventoryError)throw new ScodeA2aError(`Native root inventory unavailable: ${inventoryError}`,'not_submitted');
      const owner = owners.get(threadId);
      if (closed || !owner?.verified || !owner.connection || owner.connection.closed || owner.error) {
        throw new ScodeA2aError(owner?.error ?? 'No authenticated Agent is attached to this loaded root', 'not_submitted');
      }
      return owner.connection;
    },
    status: () => [...owners.values()].map(owner => {
      const input=owner.connection?.receiveHealth, delivery=owner.callback?.status();
      return {threadId:owner.binding.threadId,actor:owner.binding.actor,
        state:owner.error ? owner.binding.state === 'pending' ? 'waiting_for_identity'
          : owner.binding.state === 'recovery_required' ? 'recovery_required' : 'disconnected'
          : delivery?.state==='cleanup_unknown' ? 'cleanup_unknown' : owner.connection?.closed ? 'disconnected'
          : delivery?.state==='blocked' ? 'delivery_blocked'
          : input?.inputOwner==='session' && ['paused','error'].includes(input.state) ? 'input_blocked'
          : owner.verified ? 'connected' : 'connecting',error:owner.error,
        ...(owner.connection&&input?{receiver:{pid:owner.connection.pid,executable:owner.connection.executable,
          sessionId:owner.connection.sessionId,input}}:{}),...(delivery?{delivery}:{})};
    }),
    close,
  };
}
