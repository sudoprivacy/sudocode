import { spawn, type ChildProcess } from 'node:child_process';
import { Duplex } from 'node:stream';
import WebSocket from 'ws';
import { timeout } from './timeout';

export interface CodexConnectionConfig {
  codexArgv: string[];
  socket?: string;
  closeTimeoutMs?: number;
  onNotificationError?: (error: unknown) => void | Promise<void>;
}

export interface CodexNotification {
  method: string;
  params?: unknown;
}

export interface CodexConnection {
  initialized: unknown;
  readonly lastNotificationError: unknown;
  readonly exited: Promise<{ code: number | null; signal: ChildProcess['signalCode'] }>;
  request<T>(method: string, params: unknown): Promise<T>;
  close(): Promise<void>;
}

export type CodexRequestOutcome = 'not_sent' | 'unknown';

/** A server error is retained, but does not by itself prove no side effect. */
export class CodexRequestError extends Error {
  constructor(readonly outcome: CodexRequestOutcome, message: string, readonly rpcError?: unknown, cause?: unknown) {
    super(message, { cause });
    this.name = 'CodexRequestError';
  }
}

export function validateCodexConnectionConfig(config: CodexConnectionConfig): void {
  if (!Array.isArray(config.codexArgv) || !config.codexArgv.length
      || config.codexArgv.some(arg => typeof arg !== 'string' || !arg || arg.includes('\0'))
      || (config.socket !== undefined && typeof config.socket !== 'string')
      || (config.closeTimeoutMs !== undefined && (!Number.isFinite(config.closeTimeoutMs) || config.closeTimeoutMs <= 0))
      || (config.onNotificationError !== undefined && typeof config.onNotificationError !== 'function')) {
    throw new Error('Invalid native Codex connection configuration');
  }
}

/** Connect to an existing native server. Never start, resume or discover one. */
export async function connectCodex(
  config: CodexConnectionConfig, onNotification?: (notification: CodexNotification) => void | Promise<void>,
): Promise<CodexConnection> {
  validateCodexConnectionConfig(config);
  const child = spawn(config.codexArgv[0], [...config.codexArgv.slice(1), 'app-server', 'proxy',
    ...(config.socket ? ['--sock', config.socket] : [])], { stdio: ['pipe', 'pipe', 'pipe'], windowsHide: true, shell: false });
  const wire = Duplex.from({ readable: child.stdout, writable: child.stdin });
  const socket = new WebSocket('ws://localhost/', { createConnection: () => wire, handshakeTimeout: 10000 });
  let stderr = '';
  child.stderr.on('data', bytes => { stderr = (stderr+String(bytes)).slice(-4096); });
  let serial = 0;
  let failure: Error | undefined;
  let closed = false;
  let stopped = false;
  let closePromise: Promise<void> | undefined;
  let lastNotificationError: unknown;
  const closePhaseMs = Math.max(1, Math.floor((config.closeTimeoutMs ?? 3000) / 3));
  const pending = new Map<number, { resolve(value: unknown): void; reject(error: Error): void; timer: ReturnType<typeof setTimeout> }>();
  const fail = (error: Error) => {
    failure ??= error;
    for (const value of pending.values()) {
      clearTimeout(value.timer);
      value.reject(new CodexRequestError('unknown', `${failure.message}; request outcome unknown`, undefined, failure));
    }
    pending.clear();
  };
  const reportNotificationError = (error: unknown) => {
    lastNotificationError = error;
    try { void Promise.resolve(config.onNotificationError?.(error)).catch(() => {}); }
    catch { /* A diagnostic observer cannot change protocol dispatch. */ }
  };
  child.on('error', fail);
  child.stdin.on('error', fail);
  const exited = new Promise<{ code: number | null; signal: ChildProcess['signalCode'] }>(resolve => {
    child.once('close', (code, signal) => {
      stopped = true;
      resolve({ code, signal });
      fail(new Error(`Codex proxy exited ${code}/${signal}: ${stderr}`));
    });
  });
  socket.on('error', fail);
  socket.on('close', () => fail(new Error('Codex proxy closed; request outcome may be unknown')));
  socket.on('message', bytes => {
    let message: { id?: number; method?: string; params?: unknown; error?: unknown; result?: unknown };
    try {
      message = JSON.parse(bytes.toString());
      if (!message || typeof message !== 'object' || Array.isArray(message)) throw new Error('Expected a JSON-RPC object');
    } catch (error) { fail(new Error(`Invalid Codex proxy response: ${String(error)}`)); return; }
    if (typeof message.method === 'string') {
      // Requests and responses have independent ID spaces. A broadcast request
      // belongs to the actual frontend: even an error reply could win its race.
      if (message.id === undefined) {
        try {
          void Promise.resolve(onNotification?.({ method: message.method, params: message.params })).catch(reportNotificationError);
        } catch (error) { reportNotificationError(error); }
      }
      return;
    }
    if (message.id === undefined) return;
    const value = pending.get(message.id);
    if (!value) return;
    if (Object.hasOwn(message, 'result') === Object.hasOwn(message, 'error')) {
      fail(new Error('Invalid Codex proxy response: expected exactly one result or error'));
      return;
    }
    pending.delete(message.id);
    clearTimeout(value.timer);
    if (Object.hasOwn(message, 'error')) {
      value.reject(new CodexRequestError('unknown', `Codex JSON-RPC error: ${JSON.stringify(message.error)}`, message.error));
    } else value.resolve(message.result);
  });
  const request: CodexConnection['request'] = <T>(method: string, params: unknown): Promise<T> => {
    if (closed || failure || socket.readyState !== WebSocket.OPEN) {
      return Promise.reject(new CodexRequestError('not_sent', failure?.message ?? 'Codex proxy is not connected', undefined, failure));
    }
    const id = ++serial;
    let payload: string;
    try {
      if (typeof method !== 'string' || !method.trim()) throw new Error('Expected a native method name');
      payload = JSON.stringify({ jsonrpc: '2.0', id, method, params });
    } catch (error) {
      return Promise.reject(new CodexRequestError('not_sent', 'Cannot encode native Codex request', undefined, error));
    }
    return new Promise<T>((resolve, reject) => {
      const timer = setTimeout(() => {
        pending.delete(id);
        reject(new CodexRequestError('unknown', `${method} timed out; outcome unknown, no retry attempted`));
      }, 15000);
      pending.set(id, { resolve: value => resolve(value as T), reject, timer });
      try { socket.send(payload, error => { if (error) fail(error); }); }
      catch (error) { fail(error instanceof Error ? error : new Error(String(error))); }
    });
  };
  const close = (): Promise<void> => {
    if (closePromise) return closePromise;
    closed = true;
    fail(new Error('Codex connection owner closed'));
    socket.terminate();
    wire.destroy();
    if (!stopped) child.kill(); // Only our spawn, never the native server or frontend.
    closePromise = (async () => {
      try { await timeout(exited, closePhaseMs, 'Codex proxy exit'); }
      catch {
        if (!stopped) child.kill('SIGKILL');
        await timeout(exited, closePhaseMs, 'Codex proxy forced exit');
      }
    })();
    return closePromise;
  };
  try {
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => reject(failure ?? new Error('Codex proxy handshake timed out')), 11000);
      socket.once('open', () => { clearTimeout(timer); resolve(); });
      socket.once('error', error => { clearTimeout(timer); reject(error); });
      child.once('error', error => { clearTimeout(timer); reject(error); });
    });
    const initialized = await request('initialize', { clientInfo: { name: 'scode-acp-client', version: '1.0.0' }, capabilities: { experimentalApi: true } });
    socket.send(JSON.stringify({ jsonrpc: '2.0', method: 'initialized' }));
    return { initialized, request, close, exited, get lastNotificationError() { return lastNotificationError; } };
  } catch (error) { await close(); throw error; }
}
