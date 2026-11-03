import type { ScodeConnection } from './scodeAcp';
import { createHash, type Hash } from 'node:crypto';

export const SCODE_A2A_METHODS = {
  send: '_sudocode/a2a/send',
  agentList: '_sudocode/a2a/agent_list',
} as const;

export interface ScodeMessage {
  to: string;
  message: string;
  summary: string;
}

/** The real tool output, not a queued-command acknowledgement or delivery claim. */
export interface ScodeA2aResult {
  content: string;
  isError: boolean;
  _meta?: unknown;
}

export class ScodeA2aError extends Error {
  constructor(
    message: string,
    readonly outcome: 'not_submitted' | 'unknown',
    readonly rpcError?: { code?: number; data?: unknown },
  ) {
    super(message);
    this.name = 'ScodeA2aError';
  }
}

function record(value: unknown): Record<string, unknown> | undefined {
  return value !== null && typeof value === 'object' && !Array.isArray(value)
    ? value as Record<string, unknown> : undefined;
}

export function supportsScodeDirectA2a(connection: ScodeConnection): boolean {
  return directCapability(connection)?.version === 1;
}

function directCapability(connection: ScodeConnection): Record<string, unknown> | undefined {
  const sudocode = record(connection.initialized._meta?.sudocode);
  return record(sudocode?.directA2a);
}

/** Native host metadata, kept distinct from the controller's own audit row. */
export function scodeNativeA2aAudit(result: ScodeA2aResult): Record<string, unknown> | undefined {
  return record(record(record(record(result._meta)?.sudocode)?.directA2a)?.audit);
}

/** Bind a real, already checked agent_list result to its owned Node session.
 * Directory entries establish neither identity nor whether a peer is online.
 */
export function requireScodeNodeIdentity(connection: ScodeConnection, result: ScodeA2aResult, actor?: string): Record<string, unknown> {
  const audit=scodeNativeA2aAudit(result);
  if(directCapability(connection)?.hostAudit!==1 || result.isError || !audit || audit.version!==1
      || audit.tool!=='agent_list' || typeof audit.actor!=='string' || !audit.actor
      || (actor!==undefined && audit.actor!==actor) || audit.sessionId!==connection.sessionId || audit.namespaceRoot!=='') {
    throw new Error('Loaded ACP actor/session does not match its authenticated Node context');
  }
  return audit;
}

function frame(hash: Hash, bytes: Buffer): void {
  const length = Buffer.alloc(8);
  length.writeBigUInt64BE(BigInt(bytes.length));
  hash.update(length).update(bytes);
}

function inputDigest(tool: string, input: Record<string, string | boolean>): string {
  const hash = createHash('sha256').update('sudocode.direct-a2a.input.v1\0');
  frame(hash, Buffer.from(tool));
  const fields = Object.keys(input).sort();
  const count = Buffer.alloc(8); count.writeBigUInt64BE(BigInt(fields.length)); hash.update(count);
  for (const key of fields) {
    frame(hash, Buffer.from(key));
    const value = input[key];
    hash.update(Buffer.from([typeof value === 'string' ? 1 : value ? 3 : 2]));
    if (typeof value === 'string') frame(hash, Buffer.from(value));
  }
  return hash.digest('hex');
}

function checkNativeAudit(connection: ScodeConnection, method: string, params: Record<string, unknown>, result: ScodeA2aResult): void {
  if (directCapability(connection)?.hostAudit !== 1) return;
  const audit = scodeNativeA2aAudit(result);
  const tool = method === SCODE_A2A_METHODS.send ? 'send' : 'agent_list';
  const input = (tool === 'send' ? params : params.activeOnly === undefined ? {} : { active_only: params.activeOnly }) as Record<string, string | boolean>;
  const hash = createHash('sha256').update('sudocode.direct-a2a.result.v1\0')
    .update(Buffer.from([Number(result.isError)]));
  frame(hash, Buffer.from(result.content));
  if (!audit || audit.version !== 1 || audit.sessionId !== connection.sessionId
      || audit.tool !== tool
      || typeof audit.actor !== 'string' || !audit.actor.trim()
      || typeof audit.namespaceRoot !== 'string'
      || typeof audit.commandId !== 'string' || !/^external-a2a-[0-9a-f-]{36}$/.test(audit.commandId)
      || audit.requestedInputSha256 !== inputDigest(tool, input)
      || (audit.effectiveInputSha256 !== undefined && (typeof audit.effectiveInputSha256 !== 'string' || !/^[0-9a-f]{64}$/.test(audit.effectiveInputSha256)))
      || audit.resultSha256 !== hash.digest('hex')) {
    throw new Error('Invalid advertised native A2A audit; execution may already have completed');
  }
}

function validateKeys(value: unknown, keys: string[]): Record<string, unknown> {
  const input = record(value);
  if (!input || Object.keys(input).some(key => !keys.includes(key))) {
    throw new ScodeA2aError('Unknown direct A2A input field', 'not_submitted');
  }
  return input;
}

async function directRequest(
  connection: ScodeConnection, method: string, params: Record<string, unknown>, timeoutMs: number,
): Promise<ScodeA2aResult> {
  if (!supportsScodeDirectA2a(connection)) {
    throw new ScodeA2aError('This scode does not advertise directA2a version 1; install a compatible scode. No prompt fallback was attempted.', 'not_submitted');
  }
  if (connection.closed) throw new ScodeA2aError('The owned scode connection is closed', 'not_submitted');
  if (!Number.isFinite(timeoutMs) || timeoutMs <= 0) {
    throw new ScodeA2aError('Direct A2A timeout must be a finite positive number', 'not_submitted');
  }
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    const response = await Promise.race([
      connection.request(method, { sessionId: connection.sessionId, ...params }),
      new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new Error(`${method} timed out`)), timeoutMs);
      }),
    ]);
    const result = record(response);
    if (!result || typeof result.isError !== 'boolean' || typeof result.content !== 'string') {
      throw new Error('Invalid direct A2A result; execution may already have completed');
    }
    checkNativeAudit(connection, method, params, response as ScodeA2aResult);
    // Do not reinterpret isError or a tool's text as proof that no append occurred.
    return response as ScodeA2aResult;
  } catch (error) {
    const remote = record(error);
    const code = typeof remote?.code === 'number' ? remote.code : undefined;
    const phase = record(record(record(remote?.data)?.sudocode)?.directA2a);
    const refused = directCapability(connection)?.errorOutcomes === 1 && code === -32602
      && phase?.version === 1 && phase.outcome === 'not_submitted';
    // Only the advertised, paired pre-submission contract is definite. A
    // generic code, forged phase or any later failure stays Unknown.
    throw new ScodeA2aError(
      `${error instanceof Error ? error.message : String(error)}; ${refused
        ? 'host refused before tool submission' : 'outcome unknown, do not automatically resend'}`,
      refused ? 'not_submitted' : 'unknown', code === undefined ? undefined : { code, data: remote?.data },
    );
  } finally { clearTimeout(timer); }
}

/** No model call, sender override, alternate mailbox, retry, or second connection. */
export async function sendScodeMessage(
  connection: ScodeConnection, message: ScodeMessage, timeoutMs = 60000,
): Promise<ScodeA2aResult> {
  const input = validateKeys(message, ['to', 'message', 'summary']);
  if (typeof input.to !== 'string' || !input.to.trim() || typeof input.message !== 'string'
      || typeof input.summary !== 'string' || !input.summary.trim()) {
    throw new ScodeA2aError('send requires a target, plain text message and nonempty summary', 'not_submitted');
  }
  return directRequest(connection, SCODE_A2A_METHODS.send, {
    to: input.to, message: input.message, summary: input.summary,
  }, timeoutMs);
}

/** Directory visibility is addressability, not evidence that a peer is online. */
export async function listScodeAgents(
  connection: ScodeConnection, options: { activeOnly?: boolean } = {}, timeoutMs = 60000,
): Promise<ScodeA2aResult> {
  const input = validateKeys(options, ['activeOnly']);
  if (input.activeOnly !== undefined && typeof input.activeOnly !== 'boolean') {
    throw new ScodeA2aError('activeOnly must be a boolean', 'not_submitted');
  }
  return directRequest(connection, SCODE_A2A_METHODS.agentList,
    input.activeOnly === undefined ? {} : { activeOnly: input.activeOnly }, timeoutMs);
}
