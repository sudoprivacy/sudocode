import type { CallToolResult } from '@modelcontextprotocol/sdk/types.js';
import { z } from 'zod';
import type { ScodeConnection, ScodePeerMessage } from './scodeAcp';

const nonempty = z.string().min(1).refine(value => !value.includes('\0'));
const offset = z.number().int().nonnegative().safe();

/** The thread and ACP identity are selected by the host, never tool arguments. */
export const scodeInboxInput = z.object({
  ack: z.array(nonempty).optional(),
  read: nonempty.optional(),
  window: offset.optional(),
  after: nonempty.optional(),
}).strict().superRefine((input, ctx) => {
  if ((!input.read && (input.window !== undefined || input.after !== undefined))
      || ((input.window ?? 0) > 0 && !input.after)
      || ((input.window ?? 0) === 0 && input.after !== undefined)) {
    ctx.addIssue({ code: 'custom', message: 'A later window requires read, window and the preceding next token' });
  }
});
export type ScodeInboxInput = z.infer<typeof scodeInboxInput>;

export interface ScodeLegacyOffer {
  eventId: string;
  message: ScodePeerMessage;
  possiblyDelivered: boolean;
}

const committed = z.enum(['committed', 'already', 'refused']);
const reason = z.enum(['lease_lost', 'position_moved', 'token_invalid']).optional();
const commit = z.union([
  z.object({ conversation: nonempty, from: offset, to: offset, status: committed, reason }),
  z.object({ legacyEventId: nonempty, status: committed, reason }),
]).superRefine((entry, ctx) => {
  if ((entry.status === 'refused') !== (entry.reason !== undefined)
      || ('from' in entry && entry.to < entry.from)) {
    ctx.addIssue({ code: 'custom', message: 'Invalid mailbox commit outcome' });
  }
});
const offer = z.object({
  text: z.string(),
  commits: z.array(commit),
  frames: z.array(z.object({
    frameId: nonempty, conversation: nonempty, deliveryIds: z.array(nonempty), bytes: offset,
    window: z.object({ index: offset, count: z.number().int().positive().safe() }).nullable(),
    complete: z.boolean(), redelivery: z.boolean(),
  }).superRefine((frame, ctx) => {
    if ((!frame.window && !frame.complete) || (frame.window && (frame.window.index >= frame.window.count
        || frame.complete !== (frame.window.index === frame.window.count - 1)))) {
      ctx.addIssue({ code: 'custom', message: 'Invalid mailbox window' });
    }
  })),
  unread: offset,
});
export type ScodeMailboxOffer = z.infer<typeof offer> & {
  /** Host metadata from this no-ACK request's first legacy entry. scode renders
   * legacy entries first, but excludes them from response.frames. Never infer
   * that every pending legacy entry fitted in this bounded presentation. */
  legacyHead?: { eventId: string; deliveryId: string };
};

/** A conversation's first unread frame: the peer, the frame's start offset and
 * its first record's `deliveryId`. Budget keys are made of heads. */
export interface ScodeMailboxHead { conversation: string; start: number; deliveryId: string }

/** Each conversation's head in an offer. Refuses a frame whose identity does
 * not carry its conversation, its original range and a delivery identity. */
export function scodeMailboxHeads(offer: ScodeMailboxOffer): ScodeMailboxHead[] {
  const heads = new Map<string, ScodeMailboxHead>();
  for (const frame of offer.frames) {
    if (!frame.frameId.startsWith(frame.conversation + ':')) throw new Error('Mailbox frame conversation differs');
    const offsets = /^:(0|[1-9][0-9]*)-(0|[1-9][0-9]*)$/.exec(frame.frameId.slice(frame.conversation.length));
    const start = Number(offsets?.[1]), end = Number(offsets?.[2]);
    if (!offsets || !Number.isSafeInteger(start) || !Number.isSafeInteger(end) || end <= start
        || !frame.deliveryIds.length) throw new Error('Mailbox frame lacks its original range or delivery identity');
    const previous = heads.get(frame.conversation);
    if (!previous || start < previous.start) heads.set(frame.conversation,
      { conversation: frame.conversation, start, deliveryId: frame.deliveryIds[0] });
  }
  return [...heads.values()];
}

/** A response error can follow partial ACK commits. It is never evidence
 * that nothing happened; preserve the peer's diagnostic without retrying. */
export class ScodeMailboxError extends Error {
  constructor(message: string, readonly rpcError?: { code: number; data?: unknown }, cause?: unknown) {
    super(message, { cause }); this.name = 'ScodeMailboxError';
  }
}

/** Bound the encoded MCP result even if every text byte needs a JSON escape.
 * peer_offer itself bounds UTF-8 text. The smaller request preserves both
 * bounds without truncating or rewriting scode's tokens or presentation. */
export function mailboxTextBudget(encodedResultBytes: number): number {
  if (!Number.isSafeInteger(encodedResultBytes) || encodedResultBytes < 4096 || encodedResultBytes > 65536) {
    throw new Error('Mailbox result budget must be a qualified value from 4096 to 65536 bytes');
  }
  const envelope = Buffer.byteLength(JSON.stringify({ content: [{ type: 'text', text: '' }] }));
  return Math.floor((encodedResultBytes - envelope) / 6);
}

export interface ScodeMailbox {
  inbox(input: ScodeInboxInput): Promise<CallToolResult>;
  /** Read structured heads without acknowledging anything or copying text to disk. */
  inspect(): Promise<ScodeMailboxOffer>;
  close(): Promise<void>;
}

/** Explicit v3 transport; no native queue operation, implicit acknowledgement,
 * reconnect, private message spool, or fallback to peerInput v2. The caller
 * owns one mailbox per binding and qualifies its native presentation budget. */
export function openScodeMailbox(connection: ScodeConnection, threadId: string, encodedResultBytes: number,
  legacy?: {
    pending(): readonly ScodeLegacyOffer[];
    committed(commits: ScodeMailboxOffer['commits']): void;
  }, maxTextBytes = mailboxTextBudget(encodedResultBytes)): ScodeMailbox {
  nonempty.parse(threadId);
  const capability = (connection.initialized._meta?.sudocode as { peerInput?: { version?: number } } | undefined)?.peerInput;
  if (capability?.version !== 3 || connection.receiveHealth.inputOwner !== 'client') {
    throw new Error('Mailbox reception requires negotiated client-owned peerInput v3');
  }
  if (!Number.isSafeInteger(maxTextBytes) || maxTextBytes < 1 || maxTextBytes > mailboxTextBudget(encodedResultBytes)) {
    throw new Error('Mailbox text budget exceeds its qualified tool-result budget');
  }
  const maxBytes = maxTextBytes;
  let chain: Promise<unknown> = Promise.resolve();
  let closed = false;
  let blocked: ScodeMailboxError | undefined;
  const read = (input: ScodeInboxInput): Promise<ScodeMailboxOffer> => {
    // Copy and validate now so a queued caller cannot mutate a later ACK.
    let validated: ScodeInboxInput;
    try { validated = scodeInboxInput.parse(input); }
    catch (error) { return Promise.reject(error); }
    const task = chain.then(async () => {
      if (closed || connection.closed) throw new Error('Mailbox owner is closed');
      if (blocked) throw blocked;
      const pending = legacy?.pending();
      const request = connection.request('_sudocode/session/peer_offer', {
        sessionId: connection.sessionId, threadId, maxBytes,
        ...(validated.ack ? { ack: validated.ack } : {}),
        ...(validated.read ? { read: { frameId: validated.read, window: validated.window ?? 0,
          ...(validated.after ? { after: validated.after } : {}) } } : {}),
        ...(pending?.length ? { legacy: pending } : {}),
      });
      const raw = await new Promise<unknown>((resolve, reject) => {
        const hasAck = Boolean(validated.ack?.length);
        const timer = setTimeout(() => {
          blocked = new ScodeMailboxError(hasAck
            ? 'Mailbox request timed out after 15000ms; ACK outcome unknown. Close the ACP owner before replacing this mailbox'
            : 'Mailbox read timed out after 15000ms; waiting for the outstanding read to settle');
          reject(blocked);
        }, 15000);
        const settled = () => {
          clearTimeout(timer);
          // A late read cannot retire a frame. Allow the next independent call
          // only after the old request has settled, without replaying it.
          if (!hasAck) blocked = undefined;
        };
        request.then(value => { settled(); resolve(value); }, error => {
          settled();
          const remote = error as { code?: unknown; data?: unknown } | null;
          reject(new ScodeMailboxError(String(error), typeof remote?.code === 'number'
            ? { code: remote.code, data: remote.data } : undefined, error));
        });
      });
      const response = offer.parse(raw);
      if (Buffer.byteLength(response.text) > maxBytes) throw new Error('scode mailbox text exceeded its requested budget');
      const result = { content: [{ type: 'text', text: response.text }] };
      if (Buffer.byteLength(JSON.stringify(result)) > encodedResultBytes) throw new Error('Encoded mailbox result exceeded its qualified budget');
      // Only scode's verified commit result may close a legacy ledger row.
      legacy?.committed(response.commits);
      const first = !validated.read && !validated.ack?.length ? pending?.[0] : undefined;
      return { ...response, ...(first ? { legacyHead: {
        eventId: first.eventId, deliveryId: first.message.deliveryId ?? first.eventId,
      } } : {}) };
    });
    chain = task.catch(() => {});
    return task;
  };
  return {
    async inbox(input) { return { content: [{ type: 'text', text: (await read(input)).text }] }; },
    inspect: () => read({}),
    async close() { closed = true; await chain; },
  };
}
