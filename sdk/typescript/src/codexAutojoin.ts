import { mkdirSync, readFileSync, renameSync, writeSync, readdirSync, openSync, fsyncSync, closeSync } from 'node:fs';
import { randomUUID } from 'node:crypto';
import * as path from 'node:path';
import { connectCodex, type CodexConnection, type CodexConnectionConfig } from './codexConnection';
import { createCodexCallback, type CodexCallback } from './codexCallback';
import { connectScode, type ScodeAcpConfig, type ScodeConnection, type SessionNotification } from './scodeAcp';
import { listScodeAgents, scodeNativeA2aAudit, ScodeA2aError } from './scodeA2a';

export interface CodexAgentClaim {
  version: 1;
  actor: string;
  operationId: string;
  threadId: string;
  workspace: string;
}

/** The operator's existing enrollment path owns authorization and credentials.
 * Return a private, durable profile with an EXISTING native ACP session. Never
 * return Node/CA authority or create another identity after an ambiguous error.
 */
export interface CodexAgentProfile {
  actor: string;
  configFile: string;
}

export interface CodexAutojoinConfig {
  native: CodexConnectionConfig;
  stateDirectory: string;
  peers: string[];
  resolveAgent: (claim: Readonly<CodexAgentClaim>) => Promise<CodexAgentProfile>;
  pollIntervalMs?: number;
  onError?: (error: unknown) => void;
}

type State = 'provisioning' | 'ready' | 'recovery_required';
interface Binding extends CodexAgentClaim {
  state: State;
  configFile?: string;
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
}
export interface CodexAutojoin {
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

function identifier(value: unknown): value is string {
  return typeof value === 'string' && /^[0-9a-f-]{36}$/.test(value);
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
  mkdirSync(config.stateDirectory, { recursive: true, mode: 0o700 });
  const bindings = new Map<string, Binding>();
  const owners = new Map<string, Owner>();
  const seenActors = new Set<string>();
  let closed = false;
  let chain = Promise.resolve();
  let native: CodexConnection;
  let inventoryError: string | undefined;
  const report = (error: unknown) => { try { config.onError?.(error); } catch { /* Observer only. */ } };
  const save = (binding: Binding) => {
    const file = path.join(config.stateDirectory, `${binding.threadId}.json`);
    const temporary = `${file}.${randomUUID()}.tmp`;
    const handle = openSync(temporary,'wx',0o600);
    try {writeSync(handle,JSON.stringify(binding)+'\n');fsyncSync(handle);} finally {closeSync(handle);}
    renameSync(temporary, file);
    if(process.platform!=='win32'){
      const directory = openSync(config.stateDirectory,'r');
      try {fsyncSync(directory);} finally {closeSync(directory);}
    }
  };
  for (const file of readdirSync(config.stateDirectory).filter(file => file.endsWith('.json'))) {
    const binding = JSON.parse(readFileSync(path.join(config.stateDirectory, file), 'utf8')) as Binding;
    if (!identifier(binding.threadId) || file !== `${binding.threadId}.json` || binding.version !== 1
        || !/^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/.test(binding.actor) || !identifier(binding.operationId)
        || !path.isAbsolute(binding.workspace) || seenActors.has(binding.actor)
        || !['provisioning', 'ready', 'recovery_required'].includes(binding.state)
        || (binding.state === 'ready' && (!binding.configFile || !path.isAbsolute(binding.configFile)))) {
      throw new Error(`Invalid or duplicate durable Codex binding: ${file}`);
    }
    if (binding.state === 'provisioning') {
      binding.state = 'recovery_required';
      save(binding);
    }
    seenActors.add(binding.actor);
    bindings.set(binding.threadId, binding);
  }
  const stopOwner = async (owner: Owner) => {
    await owner.callback?.close();
    await owner.connection?.close();
  };
  const open = async (thread: Thread) => {
    let binding = bindings.get(thread.id);
    if (!binding) {
      binding = { version: 1, actor: `codex-${randomUUID().replaceAll('-', '')}`,
        operationId: randomUUID(), threadId: thread.id, workspace: thread.cwd, state: 'provisioning' };
      // Persist the original operation BEFORE asking any enrollment authority.
      save(binding);
      bindings.set(thread.id, binding);
    }
    const owner: Owner = { binding };
    owners.set(thread.id, owner);
    try {
      if (binding.workspace !== thread.cwd) throw new Error('Native root workspace differs from its durable binding');
      if (binding.state === 'recovery_required') throw new Error('Prior enrollment outcome is unknown; operator recovery of the original actor/operation is required');
      if (binding.state === 'provisioning') {
        try {
          const profile = await config.resolveAgent(Object.freeze({ version: 1, actor: binding.actor,
            operationId: binding.operationId, threadId: binding.threadId, workspace: binding.workspace }));
          if (profile.actor !== binding.actor || !path.isAbsolute(profile.configFile)) throw new Error('Enrollment returned another actor or a nonabsolute profile');
          binding.configFile = profile.configFile;
          binding.state = 'ready';
          save(binding);
        } catch (error) {
          binding.state = 'recovery_required';
          save(binding);
          throw error;
        }
      }
      const profile = JSON.parse(readFileSync(binding.configFile!, 'utf8')) as ScodeAcpConfig;
      if (profile.workspace !== thread.cwd || !profile.acpSessionId?.trim()) {
        throw new Error('Autoconnect requires this root workspace and an existing ACP session; no fresh-session fallback');
      }
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
        owner.error = `Owned scode exited ${exit.code}/${exit.signal}; no automatic task or send replay`;
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
        if (!loaded.has(id)) { owners.delete(id); await stopOwner(owner); }
      }
      for (const id of loaded) {
        if (closed || owners.has(id)) continue;
        const {thread} = await native.request<{thread: Thread}>('thread/read', {threadId:id,includeTurns:false});
        if (thread.id !== id || !path.isAbsolute(thread.cwd)) throw new Error('Native inventory identity mismatch');
        if (rootThread(thread)) await open(thread);
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
    const results = await Promise.allSettled([...owners.values()].map(stopOwner));
    await native.close();
    const error = results.find(result => result.status === 'rejected');
    if(error?.status==='rejected')throw error.reason;
  })();
  void native.exited.then(()=>close()).catch(report);
  return {
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
      state:owner.error ? owner.binding.state === 'recovery_required' ? 'recovery_required' : 'disconnected' : 'connected',error:owner.error})),
    close,
  };
}
