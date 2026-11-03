import { spawn } from 'node:child_process';
import { realpathSync, statSync } from 'node:fs';
import * as path from 'node:path';
import { createInterface } from 'node:readline';
import { identifier } from './codexBindings';
import { CodexRequestError, validateCodexConnectionConfig, type CodexConnectionConfig } from './codexConnection';
import { codexPeerData } from './codexPeerData';
import type { SessionNotification } from './scodeAcp';
import { timeout } from './timeout';

export interface CodexNativeQueueConfig extends CodexConnectionConfig {
  /** Explicit existing native home; never inferred from the scode workspace. */
  nativeHome: string;
  onProducerObservation?: (entry: Readonly<Record<string, unknown>>) => void | Promise<void>;
}

export interface CodexNativeQueueReceipt {
  method: 'thread/queue/add';
  transport: 'native-queue';
  outcome: 'queued';
  threadId: string;
  clientUserMessageId: string;
  queueItemId: string;
  producerPid: number;
  producerExit: { code: number | null; signal: NodeJS.Signals | null };
}

export function validateCodexNativeQueueConfig(config: CodexNativeQueueConfig): string {
  validateCodexConnectionConfig(config);
  if (config.socket !== undefined || !path.isAbsolute(config.codexArgv[0])
      || typeof config.nativeHome !== 'string' || !path.isAbsolute(config.nativeHome)
      || !statSync(config.nativeHome).isDirectory()
      || (config.onProducerObservation !== undefined && typeof config.onProducerObservation !== 'function')) {
    throw new Error('Native queue requires an absolute native command and explicit existing home, without a proxy socket');
  }
  const home = realpathSync.native(config.nativeHome);
  if (config.env?.CODEX_HOME !== undefined && realpathSync.native(config.env.CODEX_HOME) !== home) {
    throw new Error('Native queue CODEX_HOME differs from its explicit native home');
  }
  return home;
}

/** One bounded producer, with no public arbitrary-RPC seam or target loading.
 * A queue receipt confirms persistence, not an accepted native turn or read.
 * Once submission is possible every failure stays Unknown; nothing is replayed.
 */
export async function enqueueCodexPeerNotification(config: CodexNativeQueueConfig, threadId: string,
  notification: SessionNotification, eventId: string): Promise<CodexNativeQueueReceipt> {
  let home: string, text: string;
  try {
    home = validateCodexNativeQueueConfig(config);
    if (!identifier(threadId) || !identifier(eventId)) throw new Error('Explicit native thread and unique event UUIDs are required');
    text = codexPeerData(notification, eventId).text;
    if (Buffer.byteLength(JSON.stringify(text)) > 256 * 1024) throw new Error('Native queue peer envelope exceeds 256 KiB');
  } catch (error) { throw new CodexRequestError('not_sent', String(error), undefined, error); }
  const observe = (entry: Record<string, unknown>) => {
    try { void Promise.resolve(config.onProducerObservation?.(entry)).catch(() => {}); }
    catch { /* Diagnostic only. */ }
  };
  const child = spawn(config.codexArgv[0], [...config.codexArgv.slice(1), 'app-server', '--listen', 'stdio://'], {
    env: { ...(config.env ?? process.env), CODEX_HOME: home }, windowsHide: true, shell: false,
    stdio: ['pipe', 'pipe', 'pipe'],
  });
  const deadline = Date.now() + 15000;
  let failure: unknown, stderr = '', outputBytes = 0, serial = 0, submitted = false;
  let selected: string | undefined;
  const exited = new Promise<{ code: number | null; signal: NodeJS.Signals | null }>(resolve => {
    child.once('close', (code, signal) => { observe({ phase: 'exited', pid: child.pid, code, signal }); resolve({code,signal}); });
  });
  const lines = createInterface({ input: child.stdout, crlfDelay: Infinity });
  const broken = (error: unknown) => { failure ??= error; lines.close(); };
  child.on('error', broken); child.stdin.on('error', broken); lines.on('error', broken);
  child.stderr.on('data', bytes => { stderr = (stderr + bytes).slice(-4096); });
  child.stdout.on('data', bytes => {
    outputBytes += bytes.length;
    if (outputBytes > 1024 * 1024) broken(new Error('Native queue producer output exceeded 1 MiB'));
  });
  const iterator = lines[Symbol.asyncIterator]();
  const frame = (line: string): Record<string, unknown> => {
    const response = JSON.parse(line) as Record<string, unknown>;
    if (!response || typeof response !== 'object') throw new Error('Invalid native queue frame');
    if (response.method !== undefined) {
      observe({ phase: 'notification', pid: child.pid, method: response.method });
      if (response.method === 'remoteControl/status/changed'
          && (response.params as {status?: unknown} | undefined)?.status !== 'disabled') {
        throw new Error('Queue producer remote control must stay disabled');
      }
      if (response.id !== undefined || response.method === 'thread/started') {
        throw new Error('Queue producer requested client action or loaded a thread');
      }
    }
    return response;
  };
  const request = async (method: string, params: unknown): Promise<unknown> => {
    if (failure) throw failure;
    const id = ++serial;
    const payload = JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n';
    observe({ phase: 'request', pid: child.pid, id, method });
    if (method === 'thread/queue/add') submitted = true;
    child.stdin.write(payload);
    for (;;) {
      const item = await timeout(iterator.next(), Math.max(1, deadline - Date.now()), 'Native queue response');
      if (failure) throw failure;
      if (item.done) throw new Error('Native queue producer closed before its response');
      const response = frame(item.value);
      if (response.method !== undefined) continue;
      if (response.id !== id || Object.hasOwn(response, 'result') === Object.hasOwn(response, 'error')) {
        throw new Error('Invalid native queue response identity or envelope');
      }
      if (Object.hasOwn(response, 'error')) throw new Error(`Native queue RPC error: ${JSON.stringify(response.error)}`);
      return response.result;
    }
  };
  const inventory = async (phase: 'before' | 'after') => {
    const result = await request('thread/loaded/list', {}) as { data?: unknown; nextCursor?: unknown };
    if (!result || !Array.isArray(result.data) || result.data.length || result.nextCursor !== null) {
      throw new Error('Queue producer must have an empty complete target inventory');
    }
    observe({ phase: 'inventory', position: phase, pid: child.pid, loaded: [] });
  };
  let closing: Promise<{code: number | null; signal: NodeJS.Signals | null}> | undefined;
  const close = () => closing ??= (async () => {
    child.stdin.end();
    const ms = Math.max(1, Math.floor((config.closeTimeoutMs ?? 3000) / 3));
    try { return await timeout(exited, ms, 'Native queue producer exit'); }
    catch {
      child.kill('SIGTERM');
      try { await timeout(exited, ms, 'Native queue producer termination'); }
      catch { child.kill('SIGKILL'); await timeout(exited, ms, 'Native queue producer forced exit'); }
      throw new Error('Native queue producer required forced termination');
    } finally { lines.close(); }
  })();
  observe({ phase: 'spawned', pid: child.pid });
  try {
    await request('initialize', { clientInfo: { name: 'scode-native-queue', version: '1.0.0' }, capabilities: { experimentalApi: true } });
    child.stdin.write(JSON.stringify({ jsonrpc: '2.0', method: 'initialized' }) + '\n');
    await inventory('before');
    const result = await request('thread/queue/add', { threadId, input: [{ type: 'text', text }], clientUserMessageId: eventId }) as {
      queuedSubmission?: { id?: unknown; clientUserMessageId?: unknown; input?: Array<{ type?: unknown; text?: unknown }> };
    };
    const item = result?.queuedSubmission;
    if (!item || !identifier(item.id) || item.clientUserMessageId !== eventId
        || !Array.isArray(item.input) || item.input.length !== 1 || item.input[0].type !== 'text' || item.input[0].text !== text) {
      throw new Error('Native queue receipt does not match the submitted event');
    }
    selected = item.id;
    await inventory('after');
    // Keep checking notifications through EOF, including after the final
    // inventory response. Late thread loading cannot become a queued receipt.
    const drain = async () => {
      for (;;) {
        const item = await timeout(iterator.next(), config.closeTimeoutMs ?? 3000, 'Native queue EOF');
        if (item.done) return;
        if (frame(item.value).method === undefined) throw new Error('Unexpected response after native queue completion');
      }
    };
    const [exit] = await Promise.all([close(), drain()]);
    if (exit.code !== 0 || exit.signal !== null || failure || !child.pid) throw new Error('Native queue producer did not exit normally');
    return { method: 'thread/queue/add', transport: 'native-queue', outcome: 'queued', threadId,
      clientUserMessageId: eventId, queueItemId: selected, producerPid: child.pid, producerExit: exit };
  } catch (error) {
    await close().catch(() => {});
    throw new CodexRequestError(submitted ? 'unknown' : 'not_sent',
      `Native queue ${submitted ? 'outcome unknown; no replay' : 'not submitted'}: ${String(error)}${stderr ? `; ${stderr}` : ''}`,
      { threadId, clientUserMessageId: eventId, producerPid: child.pid, queueItemId: selected }, error);
  }
}
