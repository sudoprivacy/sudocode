import { appendFileSync, readFileSync } from 'node:fs';
import * as path from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { connectScode, ScodeCleanupUnknownError, type ScodeAcpConfig, type ScodeConnection, type ScodePeerMessage, type ScodePeerUnread } from './scodeAcp';
import type { ScodeMailbox } from './scodeMailbox';
import { listScodeAgents, requireScodeNodeIdentity, ScodeA2aError } from './scodeA2a';
import { registerScodeMcpTools } from './mcpTools';
import { registerCommandShutdown } from './lifecycle';
import { connectCodexCallback, type CodexCallback, type CodexCallbackConfig } from './codexCallback';
import { CodexCleanupUnknownError } from './codexConnection';
import { acquireReceiverOwner, validateReceiverOwner, ReceiverOwnerCleanupUnknownError, type ReceiverOwnerIdentity, type ReceiverOwnerLease } from './receiverOwner';
import type { ClaudeDelivery } from './claudeDelivery';

export interface ScodeMcpConfig extends ScodeAcpConfig {
  version: 1;
  auditLog: string;
  codexCallback?: CodexCallbackConfig;
  /** Optional binding for a receiver attached to an existing Windows frontend.
   * SID/callback identity stays in this same config, never in a guard sidecar. */
  receiverOwner?: ReceiverOwnerIdentity;
}

function readConfig(file: string): ScodeMcpConfig {
  const config = JSON.parse(readFileSync(path.resolve(file), 'utf8')) as ScodeMcpConfig;
  if (!config || config.version !== 1 || typeof config.acpSessionId !== 'string' || !config.acpSessionId.trim()
      || typeof config.auditLog !== 'string' || !path.isAbsolute(config.auditLog)) {
    throw new Error('MCP config requires version:1, an existing acpSessionId and an absolute auditLog');
  }
  if (config.receiverOwner !== undefined) validateReceiverOwner(config.receiverOwner);
  return config;
}

/** What serving needs before it is attached: where to audit and how peers are delivered. */
export interface ScodeMcpFrame {
  auditLog: string;
  /** The MCP client (Claude Code) is the agent its peers address: this host
   * owns the session's mailbox (peerInput v3) and delivers its offers. */
  claude?: ClaudeDelivery;
}

const ATTACH_RETRY_MAX_MS = 60_000;

/** One MCP stdio server owns one existing native session and its full CLI. */
export async function runScodeMcp(file: string, lifecycle: {
  registerShutdown?: typeof registerCommandShutdown;
} = {}): Promise<void> {
  const config = readConfig(file);
  await serveScodeMcp({ auditLog: config.auditLog }, async () => config, lifecycle);
}

/**
 * Serve MCP at once and attach to the agent network in the background, with
 * bounded backoff, the way a Codex owner keeps refreshing its threads: a session
 * that starts while its Node or enrollment authority is unreachable joins once
 * they return. Tool calls refuse as not_submitted while attachment is unavailable.
 * Once a profile is resolved, reconnection retains that exact session and waits for
 * joined child cleanup. In-flight Unknown commands are never replayed.
 */
export async function serveScodeMcp(frame: ScodeMcpFrame, resolve: () => Promise<ScodeMcpConfig>, lifecycle: {
  registerShutdown?: typeof registerCommandShutdown;
} = {}): Promise<void> {
  const audit = (entry: Record<string, unknown>) => appendFileSync(frame.auditLog,
    JSON.stringify({ at: new Date().toISOString(), provenance: 'external-command', ...entry })+'\n', { mode: 0o600 });
  // Check the audit destination before spawning an owner or submitting a command.
  audit({ phase: 'starting' });
  const claude = frame.claude;
  let config: ScodeMcpConfig | undefined;
  const server = new McpServer({ name: 'nexus-scode', version: '1.0.0' });
  const abort = new AbortController();
  let scode: ScodeConnection | undefined;
  let mailbox: ScodeMailbox | undefined;
  let callback: CodexCallback | undefined;
  let ownerTask: Promise<void> | undefined;
  let ownerLease: ReceiverOwnerLease | undefined;
  let cleanupUnknown: unknown;
  let ownerError: string | undefined;
  let initialized = false;
  let observed: { updates: unknown[]; bytes: number; truncated: boolean } | undefined;
  let stopping: Promise<void> | undefined;
  let finish = () => {};
  const finished = new Promise<void>(resolve => { finish = resolve; });
  const stop = () => {
    stopping ??= (async () => {
      abort.abort();
      await ownerTask;
      const results = await Promise.allSettled([callback?.close(), scode?.close()]);
      const rejected = results.find(result => result.status === 'rejected');
      if (rejected?.status === 'rejected') cleanupUnknown = rejected.reason;
      if (cleanupUnknown) throw cleanupUnknown;
      // Graceful shutdown includes the ownership lease, not just the MCP
      // transport. The signal adapter must await its actual cleanup outcome.
      await ownerLease?.close();
      // Keep the MCP endpoint available on UNKNOWN. Closing it first can ask
      // the frontend to dispose this process before ownership is reconciled.
      await server.close();
    })().finally(finish);
    return stopping;
  };
  const removeShutdown = (lifecycle.registerShutdown ?? registerCommandShutdown)(stop);
  const onEof = () => { void stop().catch(error => process.stderr.write(`${String(error)}\n`)); };
  process.stdin.once('end', onEof);
  try {
    // Only this already-bound session may reconnect. A command submitted on a
    // dead connection is never replayed; the callback's durable inbox survives.
    ownerTask = (async () => {
      let attachRetryMs = 1000;
      while (!abort.signal.aborted && !config) {
        try { config = await resolve(); }
        catch (error) {
          ownerError = String(error);
          audit({ phase: 'attach_error', error: ownerError });
          await delay(attachRetryMs, undefined, { signal: abort.signal }).catch(error => {
            if (!abort.signal.aborted) throw error;
          });
          attachRetryMs = Math.min(ATTACH_RETRY_MAX_MS, attachRetryMs * 2);
        }
      }
      if (abort.signal.aborted || !config) return;
      if (config.receiverOwner) {
        ownerLease = await acquireReceiverOwner(config.receiverOwner, config.acpSessionId!, abort.signal);
        audit({ phase: 'receiver_owner_bound', identity: config.receiverOwner, monitorPid: ownerLease.monitorPid });
        void ownerLease.ended.then(result => {
          // A monitor error means UNKNOWN, never proof that the frontend died.
          try { audit({ phase: 'receiver_owner_ended', ...result }); }
          catch (error) { process.stderr.write(`Owner observation persistence failed: ${String(error)}\n`); }
          void stop().catch(error => process.stderr.write(`${String(error)}\n`));
        });
      }
      let retryMs = 5000;
      while (!abort.signal.aborted) {
        let candidate: ScodeConnection | undefined;
        try {
          audit({ phase: 'connecting', sessionId: config.acpSessionId });
          if (!callback && config.codexCallback) callback = await connectCodexCallback(config.codexCallback, config.acpSessionId!);
          if (abort.signal.aborted) break;
          // A Codex callback or a Claude Code session is the agent its peers
          // address, so scode hands their input over instead of answering it.
          const peerOwner = callback;
          candidate = await connectScode({ ...config, signal: abort.signal,
            ...(claude ? { peerInputVersion: 3 as const, onPeerUnread: (event: ScodePeerUnread) => claude.unread(event.unread) }
              : peerOwner ? { peerInputVersion: 2 as const, onPeerMessage: (message: ScodePeerMessage) => {
              peerOwner.receive(message);
            } } : {}) }, notification => {
            if (observed && notification.update.sessionUpdate !== 'agent_thought_chunk') {
              const bytes = Buffer.byteLength(JSON.stringify(notification));
              if (observed.bytes+bytes <= 512 * 1024) { observed.updates.push(notification); observed.bytes += bytes; }
              else observed.truncated = true;
            }
            // MCP delivery is an event, not a prompt result. This notification alone
            // does not claim to wake an idle Codex frontend; its callback owner does.
            if (initialized) void server.server.notification({ method: 'notifications/scode/session_update', params: { notification } })
              .catch(error => process.stderr.write(`scode notification: ${String(error)}\n`));
          });
          if (claude) {
            // Offers span the whole Node namespace: open the mailbox only after
            // the session proves it is the bound Agent on its Node.
            requireScodeNodeIdentity(candidate, await listScodeAgents(candidate, {}), claude.actor);
            if (abort.signal.aborted) break;
            mailbox = claude.attach(candidate);
          }
          if (abort.signal.aborted) break;
          audit({ phase: 'connected', sessionId: candidate.sessionId, pid: candidate.pid });
          scode = candidate;
          ownerError = undefined;
          const exit = await candidate.exited;
          scode = undefined;
          ownerError = `scode exited ${exit.code}/${exit.signal}`;
          audit({ phase: 'child_exited', sessionId: candidate.sessionId, pid: candidate.pid, ...exit });
          retryMs = 5000;
        } catch (error) {
          ownerError = String(error);
          process.stderr.write(`scode owner: ${ownerError}\n`);
          if (error instanceof ScodeCleanupUnknownError || error instanceof CodexCleanupUnknownError) throw error;
          audit({ phase: 'connection_failed', sessionId: config.acpSessionId, error: ownerError });
        } finally {
          scode = undefined;
          mailbox = undefined;
          // An acknowledgement already sent to scode settles before its transport closes.
          await claude?.detach();
          // A failed close stops this supervisor; it must not make a second owner.
          try { await candidate?.close(); }
          catch (error) { cleanupUnknown = error; throw error; }
        }
        await delay(retryMs, undefined, { signal: abort.signal }).catch(error => {
          if (!abort.signal.aborted) throw error;
        });
        retryMs = Math.min(30000, retryMs * 2);
      }
    })().catch(error => {
      ownerError = String(error);
      // The original process must remain the exclusive receiver owner when a
      // child close was not observed. Stopping MCP must not release its lock
      // and silently permit a replacement while that child might still live.
      if (error instanceof ScodeCleanupUnknownError || error instanceof CodexCleanupUnknownError
          || error instanceof ReceiverOwnerCleanupUnknownError) cleanupUnknown = error;
      process.stderr.write(`scode recovery stopped: ${ownerError}\n`);
    });
    // MCP initialization must not wait for ACP/native availability. A slow
    // first attempt can exceed the frontend's MCP startup timeout and kill the
    // very supervisor that should recover it. Tools expose not_submitted until
    // the original session is connected; commands are never buffered/replayed.
    if (abort.signal.aborted) return;
    let promptActive = false;
    registerScodeMcpTools(server, () => {
      claude?.called();
      if (!scode || scode.closed) throw new ScodeA2aError(
        `Not attached to the agent network yet (${ownerError ?? 'attaching'}); original session recovery continues in the background`, 'not_submitted');
      return { connection: scode, auditLog: frame.auditLog, ...(claude ? { nativeThreadId: claude.threadId, mailbox } : {}) };
    }, async (connection, text) => {
      if (promptActive) return { isError: true, content: [{ type: 'text' as const, text: 'busy: a foreground prompt is already running' }] };
      promptActive = true;
      observed = { updates: [], bytes: 0, truncated: false };
      try {
        const result = await connection.request('session/prompt', {
          sessionId: connection.sessionId, prompt: [{ type: 'text', text }],
        });
        return { content: [{ type: 'text' as const, text: JSON.stringify({ result,
          // Native background notifications can overlap a foreground prompt.
          // This bounded observation is not a correlated final answer.
          observedNotifications: observed.updates, truncated: observed.truncated,
        }) }] };
      } catch (error) {
        return { isError: true, content: [{ type: 'text' as const,
          text: `${String(error)}; inspect actual tool receipts before retrying a task with side effects` }] };
      } finally { promptActive = false; observed = undefined; }
    }, () => {
      claude?.called();
      return { pid: scode?.pid, acpSessionId: config?.acpSessionId, closed: !scode || scode.closed,
        ...(!config ? { outcome: 'not_submitted', error: `Not attached to the agent network yet (${ownerError ?? 'attaching'})` }
          : { error: ownerError }), notifications: 'recovering-same-session', automaticCommandRetry: false,
        ...(claude ? { delivery: claude.status() } : {}) };
    }, !!claude);
    server.server.onclose = onEof;
    server.server.oninitialized = () => { initialized = true; };
    await server.connect(new StdioServerTransport());
    await finished;
  } finally {
    let cleanupComplete = false;
    try { await stop(); cleanupComplete = true; }
    finally {
      // Keep the same rejected shutdown registered while an OS lock remains;
      // a later signal cannot treat an absent handler as successful cleanup.
      if (cleanupComplete) removeShutdown();
      else process.stderr.write('Receiver ownership retained: cleanup UNKNOWN\n');
      process.stdin.removeListener('end', onEof);
    }
  }
}
