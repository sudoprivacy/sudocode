import { appendFileSync, readFileSync } from 'node:fs';
import * as path from 'node:path';
import { McpServer } from '@modelcontextprotocol/sdk/server/mcp.js';
import { StdioServerTransport } from '@modelcontextprotocol/sdk/server/stdio.js';
import { connectScode, type ScodeAcpConfig, type ScodeConnection } from './scodeAcp';
import { registerScodeMcpTools } from './mcpTools';
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
    registerScodeMcpTools(server, () => ({connection,auditLog:config.auditLog}), async (_, text) => {
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
