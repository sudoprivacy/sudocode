import { spawn, type ChildProcess } from 'node:child_process';
import { appendFileSync } from 'node:fs';
import * as path from 'node:path';
import { Readable, Writable } from 'node:stream';
import { ReadableStream, TransformStream, WritableStream } from 'node:stream/web';
import * as acp from '@agentclientprotocol/sdk';
import { timeout } from './timeout';

export type {
  ContentBlock, InitializeResponse, PromptResponse, RequestPermissionRequest,
  RequestPermissionResponse, SessionNotification,
} from '@agentclientprotocol/sdk';

export interface ScodeQuestionRequest {
  sessionId: string;
  toolCallId: string;
  title?: string | null;
  description?: string | null;
  descriptionFormat?: string;
  questions: Array<{
    id: string;
    prompt: string;
    kind: string;
    required: boolean;
    allowCustomInput: boolean;
    customInputHint?: string | null;
    options: Array<{ label: string; value: string; description?: string | null; recommended: boolean }>;
  }>;
}

export interface ScodeQuestionResponse {
  answers: Array<{ id: string; value: string; label?: string | null }>;
}

export function validateScodePermissionResponse(request: acp.RequestPermissionRequest, response: acp.RequestPermissionResponse): void {
  const outcome = response?.outcome;
  if (!outcome || !['cancelled','selected'].includes(outcome.outcome)) {
    throw new Error('Permission response must be selected or cancelled');
  }
  if (outcome.outcome === 'selected' && !request.options.some(option => option.optionId === outcome.optionId)) {
    throw new Error('Permission response must select an offered optionId');
  }
}

export function validateScodeQuestionResponse(request: ScodeQuestionRequest, response: ScodeQuestionResponse): void {
  if (!response || !Array.isArray(response.answers)) throw new Error('Question response requires answers');
  const answered = new Set<string>();
  for (const answer of response.answers) {
    const question = request.questions.find(candidate => candidate.id === answer?.id);
    if (!question || answered.has(answer.id)) throw new Error('Question response contains an unknown or duplicate answer id');
    answered.add(answer.id);
    if (typeof answer.value !== 'string' || (question.required && !answer.value.trim())) throw new Error('Required question answer must not be empty');
    // Other pinned scode question kinds carry opaque strings, with no ACP
    // array/boolean encoding contract to normalize or invent here.
    if (question.kind === 'single_select' && question.options.length && !question.allowCustomInput
        && !question.options.some(option => option.value === answer.value)) {
      throw new Error('Question response must select an offered value');
    }
  }
  if (request.questions.some(question => question.required && !answered.has(question.id))) throw new Error('Question response is missing a required answer');
}

export interface ScodeAcpConfig {
  scodeArgv: string[];
  workspace: string;
  env?: Record<string, string | null | undefined>;
  /** A supplied native ID must load successfully; never fall back to session/new. */
  acpSessionId?: string;
  startupTimeoutMs?: number;
  closeTimeoutMs?: number;
  /** Aborting this owner signal stops its child, including pending initialization. */
  signal?: AbortSignal;
  protocolLog?: string;
  stderrLog?: string;
  onProtocol?: (direction: 'incoming' | 'outgoing', message: unknown) => void;
  onError?: (error: unknown) => void;
  onPermission?: (request: acp.RequestPermissionRequest, signal: AbortSignal) => Promise<acp.RequestPermissionResponse>;
  onQuestion?: (request: ScodeQuestionRequest, signal: AbortSignal) => Promise<ScodeQuestionResponse>;
}

export interface ScodeConnection {
  readonly pid: number;
  readonly sessionId: string;
  readonly closed: boolean;
  readonly initialized: acp.InitializeResponse;
  readonly exited: Promise<{ code: number | null; signal: ChildProcess['signalCode'] }>;
  request<T = unknown>(method: string, params?: unknown): Promise<T>;
  notify(method: string, params?: unknown): Promise<void>;
  /** Explicitly close this connection's owned CLI, not another agent or the Nexus node. */
  close(): Promise<void>;
}

function duration(value: number | undefined, fallback: number): number {
  if (value === undefined) {
    return fallback;
  }
  if (!Number.isFinite(value) || value <= 0) {
    throw new Error('ACP timeout must be a finite positive number');
  }
  return value;
}

async function abortable<T>(operation: (signal: AbortSignal) => Promise<T>, signals: AbortSignal[]): Promise<T> {
  const controller = new AbortController();
  const abort = () => controller.abort();
  for (const signal of signals) {
    signal.addEventListener('abort', abort, { once: true });
    if (signal.aborted) {
      controller.abort();
    }
  }
  let rejectAbort: (error: Error) => void = () => {};
  const cancelled = new Promise<never>((_, reject) => { rejectAbort = reject; });
  const reject = () => rejectAbort(new Error('ACP input request cancelled'));
  controller.signal.addEventListener('abort', reject, { once: true });
  try {
    if (controller.signal.aborted) {
      throw new Error('ACP input request cancelled');
    }
    return await Promise.race([Promise.resolve().then(() => operation(controller.signal)), cancelled]);
  } finally {
    for (const signal of signals) {
      signal.removeEventListener('abort', abort);
    }
    controller.signal.removeEventListener('abort', reject);
  }
}

function parseQuestion(value: unknown): ScodeQuestionRequest {
  if (!value || typeof value !== 'object' || !('sessionId' in value) || typeof value.sessionId !== 'string'
      || !('toolCallId' in value) || typeof value.toolCallId !== 'string'
      || !('questions' in value) || !Array.isArray(value.questions)) {
    throw new Error('Invalid _scode/ask_user_question request');
  }
  return value as ScodeQuestionRequest;
}

/**
 * One official-SDK connection for the complete CLI lifetime. Both Hydra and
 * standalone controller bridges import this implementation. ACP owns framing,
 * response correlation and dispatch; notifications are never prompt results.
 */
export async function connectScode(
  config: ScodeAcpConfig,
  onNotification: (notification: acp.SessionNotification) => void = () => {},
): Promise<ScodeConnection> {
  if (!Array.isArray(config.scodeArgv) || !config.scodeArgv.length || !config.scodeArgv[0]
      || config.scodeArgv.some(arg => typeof arg !== 'string' || arg.includes('\0'))
      || typeof config.workspace !== 'string' || config.workspace.includes('\0') || !path.isAbsolute(config.workspace)) {
    throw new Error('scodeArgv and an absolute workspace are required');
  }
  if (config.acpSessionId !== undefined && (typeof config.acpSessionId !== 'string' || !config.acpSessionId.trim())) {
    throw new Error('A supplied native session ID must be a nonempty string');
  }
  if ((config.onPermission !== undefined && typeof config.onPermission !== 'function')
      || (config.onQuestion !== undefined && typeof config.onQuestion !== 'function')) {
    throw new Error('ACP input handlers must be functions');
  }
  const startupMs = duration(config.startupTimeoutMs, 30000);
  const closeMs = duration(config.closeTimeoutMs, 3000);
  const closePhaseMs = Math.max(1, Math.floor(closeMs / 3));
  if (config.signal?.aborted) throw new Error('scode ACP connection cancelled before startup');
  const env = { ...process.env };
  for (const [key, value] of Object.entries(config.env ?? {})) {
    if (value === undefined || value === null) {
      delete env[key];
    } else {
      if (typeof value !== 'string') throw new Error('ACP environment values must be strings or null');
      env[key] = value;
    }
  }
  const child = spawn(config.scodeArgv[0], config.scodeArgv.slice(1), {
    cwd: config.workspace, env, stdio: ['pipe','pipe','pipe'], windowsHide: true, shell: false,
  });
  const closing = new AbortController();
  let stopped = false;
  let closePromise: Promise<void> | undefined;
  let connection: acp.ClientConnection | undefined;
  let rejectFailure: (error: Error) => void = () => {};
  const failure = new Promise<never>((_, reject) => { rejectFailure = reject; });
  // A spawn failure may precede the caller's first await.
  void failure.catch(() => {});
  let resolveExit: (result: { code: number | null; signal: ChildProcess['signalCode'] }) => void = () => {};
  const exited = new Promise<{ code: number | null; signal: ChildProcess['signalCode'] }>(resolve => { resolveExit = resolve; });
  const report = (error: unknown) => {
    try {
      if (config.onError) {
        config.onError(error);
      } else {
        process.emitWarning(`scode ACP diagnostic: ${String(error)}`);
      }
    } catch { /* A diagnostic observer cannot alter protocol dispatch. */ }
  };
  child.on('error', error => rejectFailure(error));
  child.on('close', (code, signal) => {
    stopped = true;
    closing.abort();
    resolveExit({ code, signal });
    const error = new Error(`scode exited ${code}/${signal}`);
    rejectFailure(error);
    connection?.close(error);
  });
  child.stdin.on('error', () => {}); // The SDK/failure promise reports a closed pipe.
  child.stderr.on('data', (bytes: Buffer) => {
    try {
      if (config.stderrLog) {
        appendFileSync(config.stderrLog, bytes, { mode: 0o600 });
      }
    } catch (error) { report(error); }
  });
  const trace = (direction: 'incoming' | 'outgoing', message: unknown) => {
    try {
      config.onProtocol?.(direction, message);
      if (config.protocolLog) {
        appendFileSync(config.protocolLog, JSON.stringify({ at: new Date().toISOString(), direction, message })+'\n', { mode: 0o600 });
      }
    } catch (error) { report(error); }
  };
  const raw = acp.ndJsonStream(Writable.toWeb(child.stdin), Readable.toWeb(child.stdout) as ReadableStream<Uint8Array>);
  type WireMessage = acp.Stream['readable'] extends ReadableStream<infer Message> ? Message : never;
  const writer = raw.writable.getWriter();
  const stream: acp.Stream = {
    readable: raw.readable.pipeThrough(new TransformStream<WireMessage, WireMessage>({
      transform(message, controller) { trace('incoming', message); controller.enqueue(message); },
    })),
    writable: new WritableStream<WireMessage>({
      write(message) { trace('outgoing', message); return writer.write(message); },
      close() { return writer.close(); },
      abort(error) { return writer.abort(error); },
    }),
  };
  let sessionId = config.acpSessionId ?? '';
  const app = acp.client({ name: 'scode-acp-client' })
    .onRequest(acp.methods.client.session.requestPermission, async ctx => {
      if (!config.onPermission) {
        return { outcome: { outcome: 'cancelled' as const } };
      }
      try {
        const response = await abortable(signal => config.onPermission!(ctx.params, signal), [ctx.signal,closing.signal]);
        validateScodePermissionResponse(ctx.params, response);
        return response;
      } catch (error) {
        if (!closing.signal.aborted && !ctx.signal.aborted) {
          report(error);
        }
        return { outcome: { outcome: 'cancelled' as const } };
      }
    })
    .onNotification(acp.methods.client.session.update, ctx => {
      try { onNotification(ctx.params); } catch (error) { report(error); }
    });
  if (config.onQuestion) {
    app.onRequest('_scode/ask_user_question', parseQuestion, ctx =>
      abortable(signal => config.onQuestion!(ctx.params, signal), [ctx.signal,closing.signal]).then(response => {
        validateScodeQuestionResponse(ctx.params, response);
        return response;
      }));
  }
  connection = app.connect(stream);
  const request = <T = unknown>(method: string, params?: unknown): Promise<T> => {
    if (stopped || closePromise) {
      return Promise.reject(new Error('scode ACP connection is closed'));
    }
    return Promise.race([connection!.agent.request<T>(method, params), failure]);
  };
  const close = (): Promise<void> => {
    if (closePromise) {
      return closePromise;
    }
    closePromise = (async () => {
      closing.abort();
      if (!stopped && sessionId) {
        try {
          await timeout(connection!.agent.request(acp.methods.agent.session.close, { sessionId }), closePhaseMs, 'ACP session/close');
        } catch (error) { report(error); }
      }
      child.stdin.end();
      try { await timeout(exited, closePhaseMs, 'scode exit'); } catch {
        child.kill('SIGKILL');
        await timeout(exited, closePhaseMs, 'scode forced exit');
      } finally {
        connection?.close();
      }
    })();
    return closePromise;
  };
  const abortOwner = () => {
    rejectFailure(new Error('scode ACP connection cancelled by its owner'));
    void close().catch(report);
  };
  const transportEnded = (error: unknown = new Error('scode ACP transport closed')) => {
    if (!stopped && !closePromise) {
      rejectFailure(error instanceof Error ? error : new Error(String(error)));
      void close().catch(report);
    }
  };
  void connection.closed.then(() => transportEnded(), transportEnded);
  config.signal?.addEventListener('abort', abortOwner, { once: true });
  child.once('close', () => config.signal?.removeEventListener('abort', abortOwner));
  if (config.signal?.aborted) abortOwner();
  try {
    const initialized = await timeout(request<acp.InitializeResponse>(acp.methods.agent.initialize, {
      protocolVersion: 1,
      clientCapabilities: { fs: { readTextFile: false, writeTextFile: false }, terminal: false },
    }), startupMs, 'ACP initialize');
    if (config.acpSessionId) {
      if (!initialized.agentCapabilities?.loadSession) {
        throw new Error('Agent cannot load the supplied native session; refusing fresh-session fallback');
      }
      await timeout(request(acp.methods.agent.session.load, { sessionId, cwd: config.workspace, mcpServers: [] }), startupMs, 'ACP session/load');
    } else {
      const created = await timeout(request<acp.NewSessionResponse>(acp.methods.agent.session.new, {
        cwd: config.workspace, mcpServers: [],
      }), startupMs, 'ACP session/new');
      sessionId = created.sessionId;
    }
    if (!sessionId || !child.pid || stopped) {
      throw new Error('scode did not establish a live native ACP session');
    }
    return {
      pid: child.pid, sessionId, initialized, exited,
      get closed() { return stopped || !!closePromise; },
      request,
      notify(method, params) {
        if (stopped || closePromise) {
          return Promise.reject(new Error('scode ACP connection is closed'));
        }
        return connection!.agent.notify(method, params);
      },
      close,
    };
  } catch (error) {
    await close().catch(report);
    throw error;
  }
}
