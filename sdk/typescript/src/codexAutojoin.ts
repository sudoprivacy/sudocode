import { randomUUID } from 'node:crypto';
import * as path from 'node:path';
import { connectCodex, type CodexConnection, type CodexConnectionConfig } from './codexConnection';
import { createCodexCallback, type CodexCallback } from './codexCallback';
import { connectScode, type ScodeConnection, type SessionNotification } from './scodeAcp';
import { listScodeAgents, scodeNativeA2aAudit, ScodeA2aError } from './scodeA2a';
import { assertWorkspace, bindProfile, boundProfile, canonicalWorkspace, identifier, readBindings,
  readProfile, writeBinding, type CodexBinding as Binding, type CodexAgentClaim } from './codexBindings';
export type { CodexAgentClaim } from './codexBindings';

/** The operator's existing enrollment path owns authorization and credentials.
 * Return a private, durable profile with an EXISTING native ACP session. Never
 * return Node/CA authority or create another identity after an ambiguous error.
 */
export interface CodexAgentProfile {
  actor: string;
  configFile: string;
}

/** Only the authorized adapter can attest that no issuance was submitted. */
export class CodexEnrollmentError extends Error {
  constructor(readonly outcome: 'not_submitted' | 'unknown', detail: string) {super(detail);}
}

export interface CodexAutojoinConfig {
  native: CodexConnectionConfig;
  stateDirectory: string;
  peers: string[];
  resolveAgent: (claim: Readonly<CodexAgentClaim>) => Promise<CodexAgentProfile>;
  pollIntervalMs?: number;
  onError?: (error: unknown) => void;
  onClosed?: () => void;
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
}
export interface CodexAutojoin {
  /** True only after closure or authoritative inventory with no qualified roots. */
  readonly canRetire: boolean;
  /** Host-supplied MCP metadata selects an already loaded root, never arguments. */
  connectionFor(threadId: unknown): ScodeConnection;
  status(): Array<{threadId: string; actor: string; state: string; error?: string}>;
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
      || typeof config.resolveAgent !== 'function'
      || (config.pollIntervalMs !== undefined && (!Number.isFinite(config.pollIntervalMs) || config.pollIntervalMs < 100))) {
    throw new Error('Invalid Codex autojoin configuration');
  }
  const bindings = readBindings(config.stateDirectory);
  const owners = new Map<string, Owner>();
  let closed = false;
  let chain = Promise.resolve();
  let native: CodexConnection;
  let inventoryError: string | undefined;
  const report = (error: unknown) => { try { config.onError?.(error); } catch { /* Observer only. */ } };
  const save = (binding: Binding, create = false) => writeBinding(config.stateDirectory, binding, create);
  for (const binding of bindings.values()) {
    if (binding.state === 'provisioning') {
      binding.state = 'recovery_required';
      save(binding);
    }
  }
  const stopOwner = async (owner: Owner) => {
    await owner.callback?.close();
    await owner.connection?.close();
  };
  const open = async (thread: Thread) => {
    let binding = bindings.get(thread.id);
    if (!binding) {
      binding = { version: 2, origin: 'enrollment', actor: `codex-${randomUUID().replaceAll('-', '')}`,
        operationId: randomUUID(), threadId: thread.id, workspace: thread.cwd,
        canonicalWorkspace: canonicalWorkspace(thread.cwd), state: 'pending' };
      // Persist the original operation BEFORE asking any enrollment authority.
      save(binding, true);
      bindings.set(thread.id, binding);
    }
    const owner: Owner = { binding };
    owners.set(thread.id, owner);
    try {
      assertWorkspace(binding,thread.cwd);
      if (binding.state === 'recovery_required') throw new Error('Prior enrollment outcome is unknown; operator recovery of the original actor/operation is required');
      if (binding.state === 'pending') {
        binding.state = 'provisioning';
        save(binding);
        try {
          const profile = await config.resolveAgent(Object.freeze({ version: 1, actor: binding.actor,
            operationId: binding.operationId, threadId: binding.threadId, workspace: binding.workspace }));
          if (profile.actor !== binding.actor || !path.isAbsolute(profile.configFile)) throw new Error('Enrollment returned another actor or a nonabsolute profile');
          binding.configFile = profile.configFile;
          bindProfile(binding, readProfile(profile.configFile));
          binding.state = 'ready';
          save(binding);
        } catch (error) {
          binding.state = error instanceof CodexEnrollmentError && error.outcome === 'not_submitted' ? 'pending' : 'recovery_required';
          if(binding.state==='pending')owner.retryAt=performance.now()+2000;
          save(binding);
          throw error;
        }
      }
      const profile = boundProfile(binding);
      let callback: CodexCallback | undefined;
      const pending: SessionNotification[] = [];
      let overflow = false;
      const connection = await connectScode(profile, notification => {
        if(callback)callback.receive(notification);
        else if(notification.update.sessionUpdate==='user_message_chunk'){
          if(pending.length<64)pending.push(notification);else overflow=true;
        }
      });
      owner.connection = connection;
      // Native audit proves which Agent was actually loaded. A copied profile
      // cannot quietly attach every root to the same credential/receiver.
      const result = await listScodeAgents(connection);
      const audit = scodeNativeA2aAudit(result);
      if (overflow || result.isError || audit?.actor !== binding.actor || audit.sessionId !== connection.sessionId
          || audit.namespaceRoot !== '') {
        throw new Error('Loaded ACP actor/session does not match the durable Codex binding');
      }
      callback = createCodexCallback(native.request, { threadId: thread.id, peers: config.peers,
        log: path.join(config.stateDirectory, `${thread.id}.callback.jsonl`) }, connection.sessionId);
      owner.callback = callback;
      for(const notification of pending)callback.receive(notification);
      void connection.exited.then(exit => {
        owner.error ??= `Owned scode exited ${exit.code}/${exit.signal}; no automatic task or send replay`;
      });
      if (closed) await stopOwner(owner);
    } catch (error) {
      owner.error = String(error);
      await stopOwner(owner).catch(report);
      report(error);
    }
  };
  const refresh = () => {
    chain = chain.then(async () => {
      if (closed) return;
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
      for (const [id, owner] of owners) {
        if (!loaded.has(id) || (owner.retryAt!==undefined && performance.now()>=owner.retryAt)) {
          owners.delete(id); await stopOwner(owner);
        }
      }
      for (const id of loaded) {
        if (closed) break;
        const owner = owners.get(id);
        if (owner?.error) continue;
        const {thread} = await native.request<{thread: Thread}>('thread/read', {threadId:id,includeTurns:false});
        if (thread.id !== id || !path.isAbsolute(thread.cwd)) throw new Error('Native inventory identity mismatch');
        if (!rootThread(thread)) {
          if (owner) {owners.delete(id);await stopOwner(owner);}
        } else if (owner) {
          try {
            assertWorkspace(owner.binding,thread.cwd);
            if (owner.binding.state === 'ready') boundProfile(owner.binding);
          }
          catch (error) {owner.error=String(error);await stopOwner(owner);report(error);}
        } else await open(thread);
      }
      inventoryError = undefined;
    }).catch(error=>{inventoryError=String(error);report(error);});
    return chain;
  };
  native = await connectCodex(config.native, notification => {
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
      await native.close();
      const error = results.find(result => result.status === 'rejected');
      if(error?.status==='rejected')throw error.reason;
    } finally {
      try {config.onClosed?.();} catch(error) {report(error);}
    }
  })();
  void native.exited.then(()=>close()).catch(report);
  return {
    get canRetire() {return closed || (!inventoryError && owners.size === 0);},
    refresh,
    connectionFor(threadId) {
      if (!identifier(threadId)) throw new ScodeA2aError('Native MCP threadId metadata is required; no sender argument or cwd inference', 'not_submitted');
      if(inventoryError)throw new ScodeA2aError(`Native root inventory unavailable: ${inventoryError}`,'not_submitted');
      const owner = owners.get(threadId);
      if (closed || !owner?.connection || owner.connection.closed || owner.error) {
        throw new ScodeA2aError(owner?.error ?? 'No authenticated Agent is attached to this loaded root', 'not_submitted');
      }
      return owner.connection;
    },
    status: () => [...owners.values()].map(owner => ({threadId:owner.binding.threadId,actor:owner.binding.actor,
      state:owner.error ? owner.binding.state === 'pending' ? 'waiting_for_identity'
        : owner.binding.state === 'recovery_required' ? 'recovery_required' : 'disconnected' : 'connected',error:owner.error})),
    close,
  };
}
