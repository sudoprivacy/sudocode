import { appendFileSync, readFileSync } from 'node:fs';
import { createHash, randomUUID } from 'node:crypto';
import * as path from 'node:path';
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { z } from 'zod';
import { connectScode, type ScodeAcpConfig, type ScodeConnection } from './scodeAcp';
import { listScodeAgents, sendScodeMessage, scodeNativeA2aAudit, supportsScodeDirectA2a, ScodeA2aError, type ScodeA2aResult } from './scodeA2a';
import { registerCommandShutdown } from './lifecycle';
import { connectCodexCallback, type CodexCallback, type CodexCallbackConfig } from './codexCallback';

export interface ScodeMcpConfig extends ScodeAcpConfig {
  version: 1;
  auditLog: string;
  codexCallback?: CodexCallbackConfig;
}

function readConfig(file: string): ScodeMcpConfig {
  const config = JSON.parse(readFileSync(path.resolve(file), 'utf8')) as ScodeMcpConfig;
  if (!config || config.version !== 1 || typeof config.acpSessionId !== 'string' || !config.acpSessionId.trim()
      || typeof config.auditLog !== 'string' || !path.isAbsolute(config.auditLog)) {
    throw new Error('MCP config requires version:1, an existing acpSessionId and an absolute auditLog');
  }
  return config;
}

function digest(value: unknown): { sha256: string; bytes: number } {
  const bytes = Buffer.from(JSON.stringify(value));
  return { sha256: createHash('sha256').update(bytes).digest('hex'), bytes: bytes.length };
}

/** One MCP stdio server owns one existing native session and its full CLI. */
export async function runScodeMcp(file: string, lifecycle: {
  registerShutdown?: typeof registerCommandShutdown;
} = {}): Promise<void> {
  const config = readConfig(file);
  const audit = (entry: Record<string, unknown>) => appendFileSync(config.auditLog,
    JSON.stringify({ at: new Date().toISOString(), provenance: 'external-command', ...entry })+'\n', { mode: 0o600 });
  // Check the audit destination before spawning an owner or submitting a command.
  audit({ phase: 'starting', sessionId: config.acpSessionId });
  const server = new McpServer({ name: 'nexus-scode', version: '1.0.0' });
  const abort = new AbortController();
  let scode: ScodeConnection | undefined;
  let callback: CodexCallback | undefined;
  let transportReady = false;
  let observed: { updates: unknown[]; bytes: number; truncated: boolean } | undefined;
  let stopping: Promise<void> | undefined;
  let finish = () => {};
  const finished = new Promise<void>(resolve => { finish = resolve; });
  const stop = () => {
    stopping ??= (async () => {
      abort.abort();
      const results = await Promise.allSettled([callback?.close(), scode?.close()]);
      await server.close();
      const rejected = results.find(result => result.status === 'rejected');
      if (rejected?.status === 'rejected') throw rejected.reason;
    })().finally(finish);
    return stopping;
  };
  const removeShutdown = (lifecycle.registerShutdown ?? registerCommandShutdown)(stop);
  const onEof = () => { void stop().catch(error => process.stderr.write(`${String(error)}\n`)); };
  process.stdin.once('end', onEof);
  try {
    if (config.codexCallback) callback = await connectCodexCallback(config.codexCallback, config.acpSessionId!);
    scode = await connectScode({ ...config, signal: abort.signal }, notification => {
      callback?.receive(notification);
      if (observed && notification.update.sessionUpdate !== 'agent_thought_chunk') {
        const bytes = Buffer.byteLength(JSON.stringify(notification));
        if (observed.bytes+bytes <= 512 * 1024) { observed.updates.push(notification); observed.bytes += bytes; }
        else observed.truncated = true;
      }
      // MCP delivery is an event, not a prompt result. This notification alone
      // does not claim to wake an idle Codex frontend; its callback owner does.
      if (transportReady) void server.server.notification({ method: 'notifications/scode/session_update', params: { notification } })
        .catch(error => process.stderr.write(`scode notification: ${String(error)}\n`));
    });
    const connection = scode;
    let promptActive = false;
    server.registerTool('scode_prompt', {
      description: 'Give a task to the owned full scode session. This may use its model. Use scode_send for explicit messages.',
      inputSchema: { text: z.string().min(1) },
    }, async ({ text }) => {
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
    });
    const external = async (method: string, input: unknown, requestId: string | number,
      action: () => Promise<ScodeA2aResult>) => {
      const id = randomUUID();
      const base = { id, requestId, method, sessionId: connection.sessionId, pid: connection.pid };
      try {
        audit({ ...base, phase: 'requested', input: digest(input) });
      } catch (error) {
        return { isError: true, content: [{ type: 'text' as const,
          text: JSON.stringify({ outcome: 'not_submitted', error: `Cannot record external command: ${String(error)}` }) }] };
      }
      try {
        const result = await action();
        try { audit({ ...base, phase: 'returned', isError: result.isError, output: digest(result),
          nativeAudit: scodeNativeA2aAudit(result) }); }
        catch (error) {
          // An audit failure after execution must never replace the real result
          // with a definite failure which might invite an automatic resend.
          process.stderr.write(`External command ${id} result audit failed: ${String(error)}\n`);
        }
        // ACP returns the native tool string. Only the MCP boundary wraps it
        // in a text block; never JSON-stringify or rewrite the tool output.
        return { content: [{ type: 'text' as const, text: result.content }], isError: result.isError,
          ...(result._meta === undefined ? {} : { _meta: result._meta as Record<string, unknown> }) };
      } catch (error) {
        const report = { outcome: error instanceof ScodeA2aError ? error.outcome : 'unknown',
          error: String(error), rpcError: error instanceof ScodeA2aError ? error.rpcError : undefined,
          automaticRetry: false };
        try { audit({ ...base, phase: 'error', ...report }); }
        catch (auditError) { process.stderr.write(`External command ${id} error audit failed: ${String(auditError)}\n`); }
        return { isError: true, content: [{ type: 'text' as const, text: JSON.stringify(report) }] };
      }
    };
    server.registerTool('scode_send', {
      description: 'Send exact plain text through the owned scode without model inference. Uses native send policy/hooks. No automatic retry; a transport error after submission is Unknown.',
      inputSchema: z.object({ to: z.string().min(1), message: z.string(), summary: z.string().min(1) }).strict(),
    }, (input, extra) => external('send', input, extra.requestId, () => sendScodeMessage(connection, input)));
    server.registerTool('scode_agent_list', {
      description: 'Read the owned scode agent directory without model inference. Visibility does not prove a peer is online.',
      inputSchema: z.object({ activeOnly: z.boolean().optional() }).strict(),
    }, (input, extra) => external('agent_list', input, extra.requestId, () => listScodeAgents(connection, input)));
    server.registerTool('scode_status', {
      description: 'Report this bridge\'s owned PID, native session and direct A2A capability. Does not claim Nexus connectivity.',
      inputSchema: {},
    }, async () => ({ content: [{ type: 'text' as const, text: JSON.stringify({
      pid: connection.pid, acpSessionId: connection.sessionId,
      closed: connection.closed,
      directA2a: supportsScodeDirectA2a(connection) ? { version: 1 } : null,
      notifications: 'connection-lifetime',
    }) }] }));
    server.server.onclose = onEof;
    await server.connect(new StdioServerTransport());
    transportReady = true;
    // Keep MCP available to return Unknown for in-flight calls and report the
    // closed owner. Closing it here could discard the very error response the
    // caller needs. Reconnection remains an explicit owner action.
    void connection.exited.then(async exit => {
      try { audit({ phase: 'child_exited', sessionId: connection.sessionId, pid: connection.pid, ...exit }); }
      finally { await callback?.close(); }
    }).catch(error => process.stderr.write(`${String(error)}\n`));
    await finished;
  } finally {
    await stop();
    removeShutdown();
    process.stdin.removeListener('end', onEof);
  }
}
